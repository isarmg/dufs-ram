mod fixtures;

use fixtures::{
    Error, TEST_ACCOUNT, TEST_PASSWORD, TEST_USER, TestServer, read_bound_url, server, tmpdir,
};

use assert_cmd::prelude::*;
use assert_fs::fixture::TempDir;
use rstest::rstest;
use std::process::{Command, Stdio};

#[rstest]
#[case(&["-b", "20.205.243.166"])]
fn bind_fails(tmpdir: TempDir, #[case] args: &[&str]) -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .args(["-p", "0"])
        .args(["--auth", TEST_ACCOUNT])
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
#[case(&[] as &[&str])]
#[case(&["--path-prefix", "/prefix"])]
fn validate_printed_urls(tmpdir: TempDir, #[case] args: &[&str]) -> Result<(), Error> {
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .arg("-p")
        .arg("0")
        .args(["--auth", TEST_ACCOUNT])
        .args(args)
        .stdout(Stdio::piped())
        .spawn()?;

    let printed_url = read_bound_url(&mut child)?;
    let port = printed_url.port().ok_or("Printed URL has no port")?;
    let uri_prefix = if args.contains(&"/prefix") {
        "prefix/".to_string()
    } else {
        String::new()
    };
    assert_eq!(printed_url.path(), format!("/{uri_prefix}"));
    let server = TestServer::new(port, tmpdir, child, uri_prefix.clone(), false);
    let session = server.login(TEST_USER, TEST_PASSWORD)?;
    let managed_url = server.url().join(&uri_prefix)?;
    server.get_with(&session, managed_url)?.error_for_status()?;

    Ok(())
}
