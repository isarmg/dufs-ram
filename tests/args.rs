//! Run file server with different args

mod fixtures;
mod utils;

use assert_cmd::prelude::*;
use assert_fs::fixture::TempDir;
use fixtures::{Error, TEST_ACCOUNT, TestServer, server, tmpdir};
use rstest::rstest;
use std::process::Command;

#[rstest]
fn runtime_environment_cannot_supply_required_account(tmpdir: TempDir) -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .env("DUFS_AUTH", "environment-user:environment-password")
        .assert()
        .stderr(predicates::str::contains(
            "At least one account is required",
        ))
        .failure();
    Ok(())
}

#[rstest]
fn regular_file_cannot_be_used_as_shared_root(tmpdir: TempDir) -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path().join("index.html"))
        .args(["--auth", TEST_ACCOUNT])
        .assert()
        .stderr(predicates::str::contains("must be a directory"))
        .failure();
    Ok(())
}

#[rstest]
fn path_prefix_index(#[with(&["--path-prefix", "xyz"])] server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}{}", server.url(), "xyz"))?;
    assert_resp_paths!(server, resp);
    Ok(())
}

#[rstest]
fn path_prefix_file(#[with(&["--path-prefix", "xyz"])] server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}{}/index.html", server.url(), "xyz"))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, "This is index.html");
    Ok(())
}

#[rstest]
fn path_prefix_reject_same_component(
    #[with(&["--path-prefix", "xyz"])] server: TestServer,
) -> Result<(), Error> {
    let resp = server.get(format!("{}xyzpublic.txt", server.url()))?;
    assert_eq!(resp.status(), 400);
    Ok(())
}

#[rstest]
fn path_prefix_reject_extra_component_text(
    #[with(&["--path-prefix", "xyz"])] server: TestServer,
) -> Result<(), Error> {
    let resp = server.get(format!("{}xyzevil/public.txt", server.url()))?;
    assert_eq!(resp.status(), 400);
    Ok(())
}
