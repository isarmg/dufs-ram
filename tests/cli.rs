//! Run cli with different args, not starting a server

mod fixtures;

use assert_cmd::prelude::*;
use fixtures::Error;
use predicates::str::contains;
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
