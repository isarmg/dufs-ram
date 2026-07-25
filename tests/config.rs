mod fixtures;
mod utils;

use assert_cmd::prelude::*;
use assert_fs::TempDir;
use fixtures::{
    Error, TEST_PASSWORD, TestServer, USER_ACCOUNT, read_bound_url, tmpdir, with_new_upload_headers,
};
use predicates::str::contains;
use reqwest::Method;
use rstest::rstest;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[rstest]
fn use_config_file(tmpdir: TempDir) -> Result<(), Error> {
    let config_path = get_config_path().display().to_string();
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .arg("-p")
        .arg("0")
        .args(["--min-free-space", "0"])
        .args(["--config", &config_path])
        .stdout(Stdio::piped())
        .spawn()?;

    let port = read_bound_url(&mut child)?
        .port()
        .ok_or("Printed URL has no port")?;
    let server = TestServer::new(port, tmpdir, child, "dufs/".to_string(), false);

    let unauthenticated = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(format!("http://localhost:{port}/dufs/index.html"))
        .header("accept", "text/html")
        .send()?;
    assert_eq!(unauthenticated.status(), 303);

    let session = server.login("user", TEST_PASSWORD)?;
    let url = format!("http://localhost:{port}/dufs/index.html");
    let resp = server.get_with(&session, &url)?;
    assert_eq!(resp.text()?, "This is index.html");

    let url = format!("http://localhost:{port}/dufs/");
    let resp = server.get_with(&session, &url)?;
    let paths = server.paths_from_page_with(&session, resp)?;
    assert!(paths.contains("dir1/"));
    assert!(!paths.contains("dir3/"));
    assert!(!paths.contains("test.txt"));

    let url = format!("http://localhost:{port}/dufs/dir1/upload.txt");
    let resp = with_new_upload_headers(
        server.request_with(&session, Method::PUT, &url),
        "Hello".len() as u64,
    )
    .body("Hello")
    .send()?;
    assert_eq!(resp.status(), 201);

    Ok(())
}

#[rstest]
fn unknown_config_field_is_rejected(tmpdir: TempDir) -> Result<(), Error> {
    let config_path = tmpdir.path().join("unknown-field.yaml");
    std::fs::write(
        &config_path,
        format!("auth:\n  - '{USER_ACCOUNT}'\nunexpected-setting: true\n"),
    )?;

    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .args(["--config", config_path.to_str().expect("UTF-8 test path")])
        .assert()
        .failure()
        .stderr(contains("unknown field `unexpected-setting`"));

    Ok(())
}

fn get_config_path() -> PathBuf {
    let mut path = std::env::current_dir().expect("Failed to get current directory");
    path.push("tests");
    path.push("data");
    path.push("config.yaml");
    path
}
