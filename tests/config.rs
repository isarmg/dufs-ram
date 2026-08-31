#[path = "support/fixtures.rs"]
mod fixtures;
#[path = "support/utils.rs"]
mod utils;

use assert_cmd::prelude::*;
use assert_fs::TempDir;
use dufs::args::{Args, build_cli};
use fixtures::{
    ADMIN_ACCOUNT, Error, TEST_PASSWORD, TestServer, USER_ACCOUNT, read_bound_url, tmpdir,
    with_new_upload_headers,
};
use predicates::str::contains;
use reqwest::Method;
use rstest::rstest;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[rstest]
fn use_config_file(tmpdir: TempDir) -> Result<(), Error> {
    let config_dir = TempDir::new()?;
    let config_path = config_dir.path().join("config.yaml");
    write_private_config(&config_path, std::fs::read(get_config_path())?)?;
    let config_path = config_path.display().to_string();
    let state_dir = private_state_dir()?;
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .arg("-p")
        .arg("0")
        .args(["--min-free-space", "0"])
        .arg("--state-dir")
        .arg(state_dir.path())
        .args(["--config", &config_path])
        .stdout(Stdio::piped())
        .spawn()?;

    let port = read_bound_url(&mut child)?
        .port()
        .ok_or("Printed URL has no port")?;
    let server = TestServer::new(port, tmpdir, child, false);

    let unauthenticated = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(format!("http://localhost:{port}/index.html"))
        .header("accept", "text/html")
        .send()?;
    assert_eq!(unauthenticated.status(), 303);

    let session = server.login("user", TEST_PASSWORD)?;
    let url = format!("http://localhost:{port}/index.html");
    let resp = server.get_with(&session, &url)?;
    assert_eq!(resp.text()?, "This is index.html");

    let url = format!("http://localhost:{port}/");
    let resp = server.get_with(&session, &url)?;
    let paths = server.paths_from_page_with(&session, resp)?;
    assert!(paths.contains("dir1/"));
    assert!(paths.contains("dir2/"));
    assert!(paths.contains("test.txt"));

    let url = format!("http://localhost:{port}/dir1/upload.txt");
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
#[case("unexpected-setting")]
fn unknown_config_field_is_rejected(tmpdir: TempDir, #[case] field: &str) -> Result<(), Error> {
    let config_path = tmpdir.path().join("unknown-field.yaml");
    write_private_config(
        &config_path,
        format!("auth:\n  - '{USER_ACCOUNT}'\n{field}: true\n"),
    )?;

    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .args(["--config", config_path.to_str().expect("UTF-8 test path")])
        .assert()
        .failure()
        .stderr(contains(format!("unknown field `{field}`")));

    Ok(())
}

#[rstest]
fn unknown_yaml_log_variable_is_rejected(tmpdir: TempDir) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let config_path = tmpdir.path().join("unknown-log-variable.yaml");
    write_private_config(
        &config_path,
        format!(
            "auth:\n  - '{USER_ACCOUNT}'\nstate-dir: '{}'\nlog-format: '$stauts'\n",
            state_dir.path().display()
        ),
    )?;
    let matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        config_path.to_str().expect("UTF-8 config path"),
    ])?;

    let error = Args::parse(matches).expect_err("unknown YAML log variable was accepted");
    assert!(
        format!("{error:#}").contains("Unknown HTTP log variable `$stauts`"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
fn duplicate_yaml_bind_address_is_rejected(tmpdir: TempDir) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let config_path = tmpdir.path().join("duplicate-bind.yaml");
    write_private_config(
        &config_path,
        format!(
            "auth:\n  - '{USER_ACCOUNT}'\nstate-dir: '{}'\nbind:\n  - 127.0.0.1\n  - 127.0.0.1\n",
            state_dir.path().display()
        ),
    )?;
    let matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        config_path.to_str().expect("UTF-8 config path"),
    ])?;

    let error = Args::parse(matches).expect_err("duplicate YAML bind address was accepted");
    assert!(
        error
            .to_string()
            .contains("bind must not contain duplicate IP addresses"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
fn deployment_yaml_example_parses(tmpdir: TempDir) -> Result<(), Error> {
    const PLACEHOLDER: &str = "admin:$argon2id$REPLACE_WITH_A_REAL_HASH";
    const STATE_DIR: &str = "/var/lib/dufs";

    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/dufs.yaml.example");
    let template = std::fs::read_to_string(example)?;
    assert_eq!(
        template.matches(PLACEHOLDER).count(),
        1,
        "deployment account placeholder must occur exactly once"
    );
    assert_eq!(
        template.matches(STATE_DIR).count(),
        1,
        "deployment state directory path must occur exactly once"
    );

    let state_root = private_state_dir()?;
    let state_dir = state_root.path().join("state dir & # \"quoted\"");
    std::fs::create_dir(&state_dir)?;
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
    let config_dir = TempDir::new()?;
    let state_dir_scalar = serde_json::to_string(&state_dir.to_string_lossy())?;
    let rendered =
        template
            .replacen(PLACEHOLDER, ADMIN_ACCOUNT, 1)
            .replacen(STATE_DIR, &state_dir_scalar, 1);
    let config = config_dir.path().join("dufs.yaml");
    write_private_config(&config, rendered)?;
    let matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        config.to_str().expect("UTF-8 test path"),
    ])?;
    let args = Args::parse(matches)?;

    assert_eq!(args.serve_path, std::fs::canonicalize(tmpdir.path())?);
    assert_eq!(args.state_dir.as_deref(), Some(state_dir.as_path()));
    assert_eq!(args.addrs, [std::net::IpAddr::from([127, 0, 0, 1])]);
    assert_eq!(
        args.trusted_proxies
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["127.0.0.1/32"]
    );
    assert_eq!(args.port, 5000);
    assert!(args.auth.has_users());
    Ok(())
}

#[rstest]
fn cli_state_dir_overrides_yaml_state_dir(tmpdir: TempDir) -> Result<(), Error> {
    let yaml_state_dir = private_state_dir()?;
    let cli_state_dir = private_state_dir()?;
    let config_dir = TempDir::new()?;
    let config = config_dir.path().join("state-dir.yaml");
    write_private_config(
        &config,
        format!(
            "auth:\n  - '{USER_ACCOUNT}'\nstate-dir: '{}'\n",
            yaml_state_dir.path().display()
        ),
    )?;

    let yaml_matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        config.to_str().expect("UTF-8 test path"),
    ])?;
    let yaml_args = Args::parse(yaml_matches)?;
    assert_eq!(yaml_args.state_dir.as_deref(), Some(yaml_state_dir.path()));

    let cli_matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        config.to_str().expect("UTF-8 test path"),
        "--state-dir",
        cli_state_dir.path().to_str().expect("UTF-8 test path"),
    ])?;
    let cli_args = Args::parse(cli_matches)?;
    assert_eq!(cli_args.state_dir.as_deref(), Some(cli_state_dir.path()));
    Ok(())
}

#[test]
fn deployment_service_provisions_a_private_state_directory() -> Result<(), Error> {
    let service = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("deploy/dufs.service");
    let service = std::fs::read_to_string(service)?;
    assert!(service.lines().any(|line| line == "StateDirectory=dufs"));
    assert!(
        service
            .lines()
            .any(|line| line == "StateDirectoryMode=0700")
    );
    Ok(())
}

#[rstest]
#[case("")]
#[case("-journal")]
fn state_dir_rejects_configuration_file_collisions(
    tmpdir: TempDir,
    #[case] suffix: &str,
) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let state_db = state_dir.path().join("state.sqlite3");
    let mut config_path = state_db.as_os_str().to_os_string();
    config_path.push(suffix);
    let config_path = PathBuf::from(config_path);
    write_private_config(
        &config_path,
        format!(
            "auth:\n  - '{USER_ACCOUNT}'\nstate-dir: '{}'\n",
            state_dir.path().display()
        ),
    )?;
    let matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        config_path.to_str().expect("UTF-8 config path"),
    ])?;

    let error = Args::parse(matches).expect_err("SQLite/config path collision was accepted");
    assert!(
        error
            .to_string()
            .contains("conflicts with SQLite state database"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

fn get_config_path() -> PathBuf {
    let mut path = std::env::current_dir().expect("Failed to get current directory");
    path.push("tests");
    path.push("data");
    path.push("config.yaml");
    path
}

fn private_state_dir() -> Result<TempDir, Error> {
    let state_dir = TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(state_dir)
}

fn write_private_config(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), Error> {
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    if let Err(error) = rustix::fs::removexattr(path, "system.posix_acl_access")
        && error != rustix::io::Errno::NODATA
        && error != rustix::io::Errno::NOTSUP
    {
        return Err(std::io::Error::from(error).into());
    }
    Ok(())
}
