//! Run cli with different args, not starting a server

#[path = "support/fixtures.rs"]
mod fixtures;

use assert_cmd::prelude::*;
use dufs::args::CLI_AUTH_REJECTION_MESSAGE;
use fixtures::Error;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::{contains, is_match};
use std::process::Command;

#[test]
/// Show help and exit.
fn help_shows() -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg("-h")
        .assert()
        .success()
        .stdout(predicates::str::contains("--auth").not())
        .stdout(predicates::str::contains("-a,").not());

    Ok(())
}

#[test]
fn command_line_auth_is_rejected_early_without_echoing_the_value() -> Result<(), Error> {
    const FAKE_SECRET: &str = "fake-user:fake-phc-secret-for-cli-rejection";
    let attached_long = format!("--auth={FAKE_SECRET}");
    let attached_short = format!("-a{FAKE_SECRET}");
    let equals_short = format!("-a={FAKE_SECRET}");
    let cases = [
        vec!["--auth".to_string(), FAKE_SECRET.to_string()],
        vec![attached_long],
        vec!["-a".to_string(), FAKE_SECRET.to_string()],
        vec![attached_short],
        vec![equals_short],
        vec!["--auth".to_string()],
        vec!["-a".to_string()],
    ];

    for args in cases {
        let output = Command::new(assert_cmd::cargo::cargo_bin!())
            .args(&args)
            .output()?;
        assert!(!output.status.success(), "accepted legacy argv: {args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(CLI_AUTH_REJECTION_MESSAGE),
            "unexpected stderr for {args:?}: {stderr}"
        );
        assert!(
            !stderr.contains(FAKE_SECRET),
            "stderr exposed the rejected auth value: {stderr}"
        );
    }
    Ok(())
}

#[test]
fn command_line_auth_is_rejected_before_config_is_opened() -> Result<(), Error> {
    const FAKE_SECRET: &str = "fake-user:fake-secret-before-config";
    let output = Command::new(assert_cmd::cargo::cargo_bin!())
        .args([
            "--config",
            "/definitely/missing/dufs-auth-rejection.yaml",
            "--auth",
            FAKE_SECRET,
        ])
        .output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(CLI_AUTH_REJECTION_MESSAGE),
        "stderr={stderr}"
    );
    assert!(!stderr.contains(FAKE_SECRET), "stderr={stderr}");
    assert!(!stderr.contains("Failed to read config"), "stderr={stderr}");
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
