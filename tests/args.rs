//! Run file server with different args

#[path = "support/fixtures.rs"]
mod fixtures;

use assert_cmd::prelude::*;
use assert_fs::fixture::TempDir;
use dufs::args::{Args, build_cli};
use fixtures::{Error, TEST_ACCOUNT, dufs_command, test_auth_config, tmpdir};
use rstest::rstest;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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
    let (mut command, _auth_config) = dufs_command(&[TEST_ACCOUNT]);
    command
        .arg(tmpdir.path().join("index.html"))
        .assert()
        .stderr(predicates::str::contains("must be a directory"))
        .failure();
    Ok(())
}

#[rstest]
#[case("--path-prefix")]
#[case("--hidden")]
#[case("--state-db")]
#[case("--compress")]
#[case("--max-zip-entries")]
#[case("--max-zip-uncompressed-size")]
#[case("--max-zip-output-size")]
#[case("--max-concurrent-zips")]
fn removed_options_are_rejected(tmpdir: TempDir, #[case] option: &str) -> Result<(), Error> {
    let (mut command, _auth_config) = dufs_command(&[TEST_ACCOUNT]);
    command
        .arg(tmpdir.path())
        .args([option, "value"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(format!(
            "unexpected argument '{option}'"
        )));
    Ok(())
}

#[rstest]
fn unknown_cli_log_variable_is_rejected(tmpdir: TempDir) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let auth_config = test_auth_config(&[TEST_ACCOUNT]);
    let matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        auth_config.path().to_str().expect("UTF-8 config path"),
        "--state-dir",
        state_dir.path().to_str().expect("UTF-8 state path"),
        "--log-format",
        "$stauts",
    ])?;
    let error = Args::parse(matches).expect_err("unknown CLI log variable was accepted");
    assert!(
        error
            .to_string()
            .contains("Unknown HTTP log variable `$stauts`"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
fn duplicate_cli_bind_address_is_rejected(tmpdir: TempDir) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let auth_config = test_auth_config(&[TEST_ACCOUNT]);
    let matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        auth_config.path().to_str().expect("UTF-8 config path"),
        "--state-dir",
        state_dir.path().to_str().expect("UTF-8 state path"),
        "--bind",
        "127.0.0.1",
        "--bind",
        "127.0.0.1",
    ])?;
    let error = Args::parse(matches).expect_err("duplicate CLI bind address was accepted");
    assert!(
        error
            .to_string()
            .contains("bind must not contain duplicate IP addresses"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
fn state_dir_is_required(tmpdir: TempDir) -> Result<(), Error> {
    let auth_config = test_auth_config(&[TEST_ACCOUNT]);
    let matches = build_cli().try_get_matches_from([
        "dufs",
        tmpdir.path().to_str().expect("UTF-8 test path"),
        "--config",
        auth_config.path().to_str().expect("UTF-8 config path"),
    ])?;
    let error = Args::parse(matches).expect_err("missing state directory was accepted");
    assert!(
        error
            .to_string()
            .contains("persistent SQLite state directory is required"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
fn state_dir_cli_derives_a_fixed_database_path_and_is_revalidatable(
    tmpdir: TempDir,
) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let args = parse_state_dir_args(tmpdir.path(), state_dir.path())?;
    assert_eq!(
        args.state_dir.as_deref(),
        Some(std::fs::canonicalize(state_dir.path())?.as_path())
    );
    let args = args.validate()?;
    assert_eq!(args.state_dir.as_deref(), Some(state_dir.path()));
    Ok(())
}

#[rstest]
fn state_dir_cli_rejects_a_missing_directory(tmpdir: TempDir) -> Result<(), Error> {
    let parent = TempDir::new()?;
    let missing = parent.path().join("missing");
    let error = parse_state_dir_args(tmpdir.path(), &missing)
        .expect_err("missing state directory was accepted");
    assert!(
        error
            .to_string()
            .contains("Failed to inspect state directory"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
#[case(false)]
#[case(true)]
fn state_dir_cli_rejects_a_symbolic_link(
    tmpdir: TempDir,
    #[case] trailing_slash: bool,
) -> Result<(), Error> {
    let parent = TempDir::new()?;
    let target = parent.path().join("target");
    let state_dir = parent.path().join("state");
    std::fs::create_dir(&target)?;
    std::os::unix::fs::symlink(&target, &state_dir)?;
    let configured = if trailing_slash {
        std::path::PathBuf::from(format!("{}/", state_dir.display()))
    } else {
        state_dir.clone()
    };

    let error = parse_state_dir_args(tmpdir.path(), &configured)
        .expect_err("symbolic-link state directory was accepted");
    assert!(
        error.to_string().contains("must not be a symbolic link"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
fn state_dir_cli_rejects_non_private_permissions(tmpdir: TempDir) -> Result<(), Error> {
    let state_dir = TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o750))?;

    let error = parse_state_dir_args(tmpdir.path(), state_dir.path())
        .expect_err("non-private state directory was accepted");
    assert!(
        error.to_string().contains("must have permissions 0700"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
fn state_dir_cli_rejects_shared_root_overlap(tmpdir: TempDir) -> Result<(), Error> {
    let nested_state = tmpdir.path().join("state");
    std::fs::create_dir(&nested_state)?;
    std::fs::set_permissions(&nested_state, std::fs::Permissions::from_mode(0o700))?;
    let error = parse_state_dir_args(tmpdir.path(), &nested_state)
        .expect_err("state directory inside the shared root was accepted");
    assert!(
        error.to_string().contains("must not overlap shared path"),
        "unexpected error: {error:#}"
    );

    let parent_state = private_state_dir()?;
    let nested_root = parent_state.path().join("shared");
    std::fs::create_dir(&nested_root)?;
    let error = parse_state_dir_args(&nested_root, parent_state.path())
        .expect_err("state directory containing the shared root was accepted");
    assert!(
        error.to_string().contains("must not overlap shared path"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
fn state_dir_cli_rejects_a_symbolic_link_fixed_database(tmpdir: TempDir) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let target = state_dir.path().join("target.sqlite3");
    std::fs::write(&target, [])?;
    std::os::unix::fs::symlink(&target, state_dir.path().join("state.sqlite3"))?;

    let error = parse_state_dir_args(tmpdir.path(), state_dir.path())
        .expect_err("symbolic-link fixed database was accepted");
    assert!(
        error.to_string().contains("must not be a symbolic link"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[rstest]
#[case("")]
#[case("-journal")]
#[case("-wal")]
#[case("-shm")]
fn state_dir_cli_rejects_fixed_database_log_collisions(
    tmpdir: TempDir,
    #[case] suffix: &str,
) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let state_db = state_dir.path().join("state.sqlite3");
    let mut log_file = state_db.as_os_str().to_os_string();
    log_file.push(suffix);
    let auth_config = test_auth_config(&[TEST_ACCOUNT]);
    let matches = build_cli().try_get_matches_from([
        OsString::from("dufs"),
        tmpdir.path().as_os_str().to_owned(),
        OsString::from("--config"),
        auth_config.path().as_os_str().to_owned(),
        OsString::from("--state-dir"),
        state_dir.path().as_os_str().to_owned(),
        OsString::from("--log-file"),
        log_file,
    ])?;

    let error = Args::parse(matches).expect_err("SQLite/log path collision was accepted");
    assert!(
        error
            .to_string()
            .contains("conflicts with SQLite state database"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

fn parse_state_dir_args(serve_path: &Path, state_dir: &Path) -> anyhow::Result<Args> {
    let auth_config = test_auth_config(&[TEST_ACCOUNT]);
    let matches = build_cli()
        .try_get_matches_from([
            OsString::from("dufs"),
            serve_path.as_os_str().to_owned(),
            OsString::from("--config"),
            auth_config.path().as_os_str().to_owned(),
            OsString::from("--state-dir"),
            state_dir.as_os_str().to_owned(),
        ])
        .expect("valid state-dir command line");
    Args::parse(matches)
}

fn private_state_dir() -> Result<TempDir, Error> {
    let state_dir = TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(state_dir)
}
