#[path = "support/fixtures.rs"]
mod fixtures;

use fixtures::{
    Error, TEST_ACCOUNT, TEST_PASSWORD, TEST_USER, TestServer, read_bound_url, server, tmpdir,
};

use assert_cmd::prelude::*;
use assert_fs::fixture::TempDir;
use reqwest::blocking::Client;
use rstest::rstest;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

fn private_state_dir() -> Result<TempDir, Error> {
    let state_dir = TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(state_dir)
}
use std::time::Duration;

#[rstest]
#[case(&["-b", "20.205.243.166"])]
fn bind_fails(tmpdir: TempDir, #[case] args: &[&str]) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .args(["-p", "0"])
        .args(["--auth", TEST_ACCOUNT])
        .arg("--state-dir")
        .arg(state_dir.path())
        .args(args)
        .assert()
        .stderr(predicates::str::contains("Failed to bind"))
        .failure();

    Ok(())
}

#[rstest]
#[case("not-an-ip-address")]
#[case("localhost")]
fn non_ip_bind_is_rejected(tmpdir: TempDir, #[case] bind: &str) -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .args(["--auth", TEST_ACCOUNT])
        .args(["--bind", bind])
        .assert()
        .stderr(predicates::str::contains("invalid value"))
        .failure();

    Ok(())
}

#[rstest]
#[case(server(&[] as &[&str]), true, false)]
#[case(server(&["-b", "0.0.0.0"]), true, false)]
#[case(server(&["-b", "127.0.0.1", "-b", "::"]), true, true)]
#[case(server(&["-b", "127.0.0.1", "-b", "::1"]), true, true)]
fn bind_ipv4_ipv6(
    #[case] server: TestServer,
    #[case] bind_ipv4: bool,
    #[case] bind_ipv6: bool,
) -> Result<(), Error> {
    assert_eq!(
        reqwest::blocking::get(format!("http://127.0.0.1:{}", server.port()).as_str()).is_ok(),
        bind_ipv4
    );
    assert_eq!(
        reqwest::blocking::get(format!("http://[::1]:{}", server.port()).as_str()).is_ok(),
        bind_ipv6
    );

    Ok(())
}

#[rstest]
fn idle_listener_does_not_starve_another_bind_when_connection_limit_is_one(
    #[with(&[
        "--bind",
        "127.0.0.1",
        "--bind",
        "127.0.0.2",
        "--max-connections",
        "1",
        "--auth",
        TEST_ACCOUNT,
    ])]
    server: TestServer,
) -> Result<(), Error> {
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let response = client
        .get(format!(
            "http://127.0.0.2:{}/__dufs__/health",
            server.port()
        ))
        .send()?;
    assert_eq!(response.status(), 200);
    Ok(())
}

#[rstest]
fn validate_printed_url(tmpdir: TempDir) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .arg("-p")
        .arg("0")
        .args(["--auth", TEST_ACCOUNT])
        .arg("--state-dir")
        .arg(state_dir.path())
        .stdout(Stdio::piped())
        .spawn()?;

    let printed_url = read_bound_url(&mut child)?;
    let port = printed_url.port().ok_or("Printed URL has no port")?;
    assert_eq!(printed_url.path(), "/");
    let server = TestServer::new(port, tmpdir, child, false);
    let session = server.login(TEST_USER, TEST_PASSWORD)?;
    server
        .get_with(&session, server.url())?
        .error_for_status()?;

    Ok(())
}
