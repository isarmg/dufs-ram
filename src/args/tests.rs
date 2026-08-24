use super::*;

use assert_fs::prelude::*;
use std::ffi::OsStr;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt as _;

const TEST_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

fn private_state_dir() -> assert_fs::TempDir {
    let state_dir = assert_fs::TempDir::new().unwrap();
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    state_dir
}

fn make_config_private(path: &Path) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    if let Err(error) = rustix::fs::removexattr(path, CONFIG_POSIX_ACL_XATTR) {
        assert!(
            error == Errno::NODATA || error == Errno::NOTSUP,
            "failed to remove an inherited config ACL: {error}"
        );
    }
}

fn yaml_path_scalar(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap()
}

fn config_snapshot(mode: u32, uid: u32, gid: u32) -> ConfigFileSnapshot {
    ConfigFileSnapshot {
        device: 1,
        inode: 2,
        mode: CONFIG_REGULAR_FILE_TYPE | mode,
        links: 1,
        uid,
        gid,
        size: 128,
        modified_seconds: 3,
        modified_nanoseconds: 4,
        changed_seconds: 5,
        changed_nanoseconds: 6,
    }
}

fn matches_with_state<I, T>(values: I, state_dir: &Path) -> ArgMatches
where
    I: IntoIterator<Item = T>,
    T: AsRef<OsStr>,
{
    let mut values = values
        .into_iter()
        .map(|value| value.as_ref().to_os_string())
        .collect::<Vec<_>>();
    let has_config = values.iter().any(|value| {
        value.to_str().is_some_and(|value| {
            value == "--config" || value == "-c" || value.starts_with("--config=")
        })
    });
    if !has_config {
        let config_path = state_dir.join("test-auth.yaml");
        if !config_path.exists() {
            let mut config = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config_path)
                .unwrap();
            writeln!(config, "auth:\n  - '{TEST_ACCOUNT}'").unwrap();
            std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        values.push("--config".into());
        values.push(config_path.into_os_string());
    }
    values.push("--state-dir".into());
    values.push(state_dir.as_os_str().to_os_string());
    build_cli().try_get_matches_from(values).unwrap()
}

#[test]
fn legacy_auth_argv_scanner_handles_non_utf8_without_panicking() {
    let unrelated = OsString::from_vec(vec![0xff, b'x']);
    reject_cli_auth_args([OsString::from("dufs"), unrelated]).unwrap();

    let attached_auth = OsString::from_vec(vec![b'-', b'a', 0xff]);
    let error = reject_cli_auth_args([OsString::from("dufs"), attached_auth])
        .expect_err("non-UTF-8 -a value was accepted");
    assert_eq!(error.to_string(), CLI_AUTH_REJECTION_MESSAGE);
}

#[test]
fn legacy_auth_argv_scanner_respects_option_boundaries() {
    reject_cli_auth_args(["dufs", "--", "--auth"]).unwrap();
    reject_cli_auth_args(["dufs", "--authfoo"]).unwrap();

    let error = reject_cli_auth_args(["dufs", "-afoo"])
        .expect_err("short -a with an attached value was accepted");
    assert_eq!(error.to_string(), CLI_AUTH_REJECTION_MESSAGE);
}

#[test]
fn test_default() {
    let state_dir = private_state_dir();
    let matches = matches_with_state([""], state_dir.path());
    let args = Args::parse(matches).unwrap();
    let cwd = Args::sanitize_path(std::env::current_dir().unwrap()).unwrap();
    assert_eq!(args.serve_path, cwd);
    assert_eq!(args.port, default_port());
    assert_eq!(args.addrs, vec![IpAddr::from([127, 0, 0, 1])]);
    assert!(args.trusted_proxies.is_empty());
    assert_eq!(args.max_upload_size, DEFAULT_MAX_UPLOAD_SIZE);
    assert_eq!(args.upload_idle_timeout, DEFAULT_UPLOAD_IDLE_TIMEOUT);
    assert_eq!(args.upload_total_timeout, DEFAULT_UPLOAD_TOTAL_TIMEOUT);
    assert_eq!(args.max_concurrent_uploads, DEFAULT_MAX_CONCURRENT_UPLOADS);
    assert_eq!(args.min_free_space, DEFAULT_MIN_FREE_SPACE);
    assert_eq!(args.max_connections, DEFAULT_MAX_CONNECTIONS);
    assert_eq!(args.max_search_entries, DEFAULT_MAX_SEARCH_ENTRIES);
    assert_eq!(
        args.max_concurrent_searches,
        DEFAULT_MAX_CONCURRENT_SEARCHES
    );
    assert_eq!(args.request_timeout, DEFAULT_REQUEST_TIMEOUT);
}

#[test]
fn trusted_proxies_accept_cli_ips_and_cidrs_and_normalize_them() {
    let state_dir = private_state_dir();
    let matches = matches_with_state(
        [
            "",
            "--trusted-proxy",
            "198.51.100.42/24,127.0.0.1",
            "--trusted-proxy",
            "2001:db8::1",
            "--trusted-proxy",
            "127.0.0.1/32",
        ],
        state_dir.path(),
    );
    let args = Args::parse(matches).unwrap();
    let networks = args
        .trusted_proxies
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        networks,
        ["127.0.0.1/32", "198.51.100.0/24", "2001:db8::1/128"]
    );
}

#[test]
fn trusted_proxy_cli_values_replace_yaml_values() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let config_file = tmpdir.child("config.yaml");
    config_file
        .write_str(&format!(
            "trusted-proxies:\n  - 10.0.0.7/24\n  - 127.0.0.1\nauth:\n  - {TEST_ACCOUNT}\n"
        ))
        .unwrap();
    make_config_private(config_file.path());

    let yaml_matches =
        matches_with_state(["", "-c", &config_file.to_string_lossy()], state_dir.path());
    let yaml_args = Args::parse(yaml_matches).unwrap();
    assert_eq!(
        yaml_args
            .trusted_proxies
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["10.0.0.0/24", "127.0.0.1/32"]
    );

    let cli_matches = matches_with_state(
        [
            "",
            "-c",
            &config_file.to_string_lossy(),
            "--trusted-proxy",
            "192.0.2.7",
        ],
        state_dir.path(),
    );
    let cli_args = Args::parse(cli_matches).unwrap();
    assert_eq!(
        cli_args
            .trusted_proxies
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["192.0.2.7/32"]
    );
}

#[test]
fn trusted_proxy_yaml_accepts_a_scalar() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let config_file = tmpdir.child("config.yaml");
    config_file
        .write_str(&format!(
            "trusted-proxies: 2001:db8::1\nauth:\n  - {TEST_ACCOUNT}\n"
        ))
        .unwrap();
    make_config_private(config_file.path());

    let matches = matches_with_state(["", "-c", &config_file.to_string_lossy()], state_dir.path());
    let args = Args::parse(matches).unwrap();
    assert_eq!(args.trusted_proxies[0].to_string(), "2001:db8::1/128");
}

#[test]
fn invalid_or_unbounded_trusted_proxy_networks_are_rejected() {
    for invalid in ["not-a-proxy", "192.0.2.1/33", "2001:db8::/129"] {
        let error = build_cli()
            .try_get_matches_from(["", "--trusted-proxy", invalid])
            .expect_err("invalid trusted proxy was accepted");
        assert!(error.to_string().contains("expected an IP or CIDR"));
    }

    for unbounded in ["0.0.0.0/0", "::/0"] {
        let state_dir = private_state_dir();
        let matches = matches_with_state(["", "--trusted-proxy", unbounded], state_dir.path());
        let error = Args::parse(matches).expect_err("unbounded trusted proxy was accepted");
        assert!(error.to_string().contains("entire IPv4 or IPv6"));
    }
}

#[test]
fn trusted_proxy_union_cannot_cover_an_entire_address_family() {
    for networks in [["0.0.0.0/1", "128.0.0.0/1"], ["::/1", "8000::/1"]] {
        let tmpdir = assert_fs::TempDir::new().unwrap();
        let state_dir = private_state_dir();
        let args = Args {
            serve_path: tmpdir.path().to_path_buf(),
            state_dir: Some(state_dir.path().to_path_buf()),
            auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
            trusted_proxies: networks
                .into_iter()
                .map(|network| network.parse().unwrap())
                .collect(),
            ..Args::default()
        };
        let error = args
            .validate()
            .expect_err("collectively unbounded trusted proxies were accepted");
        assert!(error.to_string().contains("entire IPv4 or IPv6"));
    }
}

#[test]
fn trusted_proxy_union_may_leave_part_of_an_address_family_untrusted() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let args = Args {
        serve_path: tmpdir.path().to_path_buf(),
        state_dir: Some(state_dir.path().to_path_buf()),
        auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
        trusted_proxies: ["0.0.0.0/1", "128.0.0.0/2"]
            .into_iter()
            .map(|network| network.parse().unwrap())
            .collect(),
        ..Args::default()
    };
    args.validate()
        .expect("a bounded trusted proxy union was rejected");
}

#[test]
fn excessive_trusted_proxy_networks_are_rejected_for_library_callers() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let args = Args {
        serve_path: tmpdir.path().to_path_buf(),
        state_dir: Some(state_dir.path().to_path_buf()),
        auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
        trusted_proxies: vec!["127.0.0.1".parse().unwrap(); MAX_TRUSTED_PROXIES + 1],
        ..Args::default()
    };
    let error = args
        .validate()
        .expect_err("an excessive trusted proxy list was accepted");
    assert!(error.to_string().contains("trusted-proxies"));
}

#[test]
fn test_args_from_empty_config_file_requires_auth() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let config_file = tmpdir.child("config.yaml");
    config_file.write_str("").unwrap();
    make_config_private(config_file.path());

    let cli = build_cli();
    let matches = cli
        .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
        .unwrap();
    let err = Args::parse(matches).unwrap_err();
    assert!(err.to_string().contains("At least one account is required"));
}

#[test]
fn oversized_config_file_is_rejected_before_parsing() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let config_file = tmpdir.child("config.yaml");
    config_file
        .write_binary(&vec![b' '; MAX_CONFIG_BYTES as usize + 1])
        .unwrap();
    make_config_private(config_file.path());

    let matches = build_cli()
        .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
        .unwrap();
    let error = Args::parse(matches).expect_err("an oversized config was accepted");
    assert!(
        error.to_string().contains("exceeds the") && error.to_string().contains("byte limit"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn symbolic_link_config_file_is_rejected() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let target = tmpdir.child("target.yaml");
    let config_file = tmpdir.child("config.yaml");
    target
        .write_str(&format!("auth:\n  - {TEST_ACCOUNT}\n"))
        .unwrap();
    std::os::unix::fs::symlink(target.path(), config_file.path()).unwrap();

    let error = read_config(config_file.path()).expect_err("a symlink config was accepted");
    assert!(
        error.to_string().contains("Failed to read config"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn config_security_accepts_only_the_documented_owner_group_and_mode_combinations() {
    let expected_uid = 1000;
    let expected_gid = 2000;
    for (mode, uid, gid) in [
        (0o400, expected_uid, 3000),
        (0o600, 0, 3000),
        (0o440, 0, expected_gid),
        (0o440, expected_uid, expected_gid),
        (0o640, 0, expected_gid),
        (0o640, expected_uid, expected_gid),
    ] {
        validate_config_security(
            config_snapshot(mode, uid, gid),
            false,
            expected_uid,
            expected_gid,
            Path::new("config.yaml"),
        )
        .unwrap();
    }

    let wrong_owner = validate_config_security(
        config_snapshot(0o600, expected_uid + 1, expected_gid),
        false,
        expected_uid,
        expected_gid,
        Path::new("config.yaml"),
    )
    .expect_err("a config owned by an unrelated user was accepted");
    assert!(wrong_owner.to_string().contains("root (uid 0)"));

    let wrong_group = validate_config_security(
        config_snapshot(0o640, 0, expected_gid + 1),
        false,
        expected_uid,
        expected_gid,
        Path::new("config.yaml"),
    )
    .expect_err("group-readable config with an unrelated group was accepted");
    assert!(wrong_group.to_string().contains("effective service group"));

    for mode in [0o000, 0o444, 0o460, 0o620, 0o660, 0o700, 0o4600] {
        let error = validate_config_security(
            config_snapshot(mode, expected_uid, expected_gid),
            false,
            expected_uid,
            expected_gid,
            Path::new("config.yaml"),
        )
        .expect_err("an undocumented config mode was accepted");
        assert!(error.to_string().contains("0400, 0440, 0600, or 0640"));
    }
}

#[test]
fn config_security_rejects_hard_links_and_extended_access_acls() {
    let mut hard_linked = config_snapshot(0o600, 1000, 2000);
    hard_linked.links = 2;
    let hard_link_error =
        validate_config_security(hard_linked, false, 1000, 2000, Path::new("config.yaml"))
            .expect_err("a multiply-linked config was accepted");
    assert!(
        hard_link_error
            .to_string()
            .contains("exactly one hard link")
    );

    let acl_error = validate_config_security(
        config_snapshot(0o600, 1000, 2000),
        true,
        1000,
        2000,
        Path::new("config.yaml"),
    )
    .expect_err("a config with an extended access ACL was accepted");
    assert!(acl_error.to_string().contains("extended POSIX access ACL"));
}

#[test]
fn config_snapshot_stability_covers_identity_content_and_security_metadata() {
    let before = config_snapshot(0o600, 1000, 2000);
    let changed = [
        ConfigFileSnapshot {
            device: before.device + 1,
            ..before
        },
        ConfigFileSnapshot {
            inode: before.inode + 1,
            ..before
        },
        ConfigFileSnapshot {
            mode: before.mode ^ 0o200,
            ..before
        },
        ConfigFileSnapshot {
            links: before.links + 1,
            ..before
        },
        ConfigFileSnapshot {
            uid: before.uid + 1,
            ..before
        },
        ConfigFileSnapshot {
            gid: before.gid + 1,
            ..before
        },
        ConfigFileSnapshot {
            size: before.size + 1,
            ..before
        },
        ConfigFileSnapshot {
            modified_seconds: before.modified_seconds + 1,
            ..before
        },
        ConfigFileSnapshot {
            modified_nanoseconds: before.modified_nanoseconds + 1,
            ..before
        },
        ConfigFileSnapshot {
            changed_seconds: before.changed_seconds + 1,
            ..before
        },
        ConfigFileSnapshot {
            changed_nanoseconds: before.changed_nanoseconds + 1,
            ..before
        },
    ];

    ensure_config_snapshot_stable(
        before,
        before,
        Path::new("config.yaml"),
        "while it was being read",
    )
    .unwrap();
    for after in changed {
        let error = ensure_config_snapshot_stable(
            before,
            after,
            Path::new("config.yaml"),
            "while it was being read",
        )
        .expect_err("a changed config snapshot was accepted");
        assert!(
            error
                .to_string()
                .contains("changed while it was being read")
        );
    }
}

#[test]
fn injected_acl_inspection_rejects_acl_and_probe_time_metadata_changes() {
    let before = config_snapshot(0o600, 1000, 2000);
    let acl_error = inspect_open_config_with(
        Path::new("config.yaml"),
        1000,
        2000,
        || Ok(before),
        || Ok(true),
    )
    .expect_err("an injected extended ACL was accepted");
    assert!(acl_error.to_string().contains("extended POSIX access ACL"));

    let after = ConfigFileSnapshot {
        mode: before.mode ^ 0o200,
        ..before
    };
    let mut snapshots = [before, after].into_iter();
    let changed_error = inspect_open_config_with(
        Path::new("config.yaml"),
        1000,
        2000,
        || {
            Ok(snapshots
                .next()
                .expect("the inspector requested two snapshots"))
        },
        || Ok(false),
    )
    .expect_err("metadata changed during ACL inspection was accepted");
    assert!(
        changed_error
            .to_string()
            .contains("changed while its security properties were being verified")
    );
}

#[test]
fn injected_reader_reuses_one_fd_and_rejects_changed_post_read_snapshot() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let config = tmpdir.child("config.yaml");
    config.write_str("auth: []\n").unwrap();
    make_config_private(config.path());
    let mut file = OpenOptions::new().read(true).open(config.path()).unwrap();

    let mut before = config_snapshot(0o600, 1000, 2000);
    before.size = "auth: []\n".len() as u64;
    let after = ConfigFileSnapshot {
        changed_nanoseconds: before.changed_nanoseconds + 1,
        ..before
    };
    let mut calls = 0;
    let mut observed_fds = Vec::new();
    let error = read_open_config(&mut file, config.path(), |opened| {
        observed_fds.push(opened.as_raw_fd());
        calls += 1;
        Ok(if calls == 1 { before } else { after })
    })
    .expect_err("metadata changed during config reading was accepted");

    assert_eq!(calls, 2);
    assert_eq!(observed_fds.len(), 2);
    assert_eq!(observed_fds[0], observed_fds[1]);
    assert!(
        error
            .to_string()
            .contains("changed while it was being read")
    );
}

#[test]
fn read_config_rejects_a_real_hard_link_and_insecure_mode() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let insecure = tmpdir.child("insecure.yaml");
    insecure.write_str("auth: []\n").unwrap();
    std::fs::set_permissions(insecure.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    let mode_error = read_config(insecure.path()).expect_err("a 0644 config was accepted");
    assert!(mode_error.to_string().contains("0400, 0440, 0600, or 0640"));

    let linked = tmpdir.child("linked.yaml");
    linked.write_str("auth: []\n").unwrap();
    make_config_private(linked.path());
    std::fs::hard_link(linked.path(), tmpdir.child("alias.yaml").path()).unwrap();
    let link_error = read_config(linked.path()).expect_err("a hard-linked config was accepted");
    assert!(link_error.to_string().contains("exactly one hard link"));
}

#[test]
fn path_identity_matches_canonical_entries_and_existing_objects() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let real_parent = tmpdir.child("real");
    std::fs::create_dir(real_parent.path()).unwrap();
    let alias_parent = tmpdir.child("alias");
    std::os::unix::fs::symlink(real_parent.path(), alias_parent.path()).unwrap();

    let direct_entry =
        PathIdentity::inspect(&real_parent.path().join("future"), "test path").unwrap();
    let aliased_entry =
        PathIdentity::inspect(&alias_parent.path().join("future"), "test path").unwrap();
    assert!(direct_entry.shares_entry_or_object_with(&aliased_entry));
    assert!(direct_entry.object.is_none());

    let object = real_parent.child("object");
    object.write_str("contents").unwrap();
    let hard_link = tmpdir.child("object-link");
    std::fs::hard_link(object.path(), hard_link.path()).unwrap();
    let direct_object = PathIdentity::inspect(object.path(), "test path").unwrap();
    let aliased_object = PathIdentity::inspect(hard_link.path(), "test path").unwrap();
    assert_ne!(direct_object.entry, aliased_object.entry);
    assert!(direct_object.shares_entry_or_object_with(&aliased_object));
}

#[test]
fn configuration_and_log_paths_cannot_resolve_into_the_shared_root() {
    let shared = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let alias_dir = assert_fs::TempDir::new().unwrap();
    let shared_alias = alias_dir.child("shared-alias");
    std::os::unix::fs::symlink(shared.path(), shared_alias.path()).unwrap();

    let config = shared.child("config.yaml");
    config
        .write_str(&format!(
            "auth:\n  - {TEST_ACCOUNT}\nstate-dir: '{}'\n",
            state_dir.path().display()
        ))
        .unwrap();
    make_config_private(config.path());
    for config_path in [
        config.path().to_path_buf(),
        shared_alias.path().join("config.yaml"),
    ] {
        let matches = build_cli()
            .try_get_matches_from([
                OsString::from("dufs"),
                shared.path().as_os_str().to_owned(),
                OsString::from("--config"),
                config_path.into_os_string(),
            ])
            .unwrap();
        let error = Args::parse(matches)
            .expect_err("a configuration path inside the shared root was accepted");
        assert!(
            error.to_string().contains("resolve into shared path"),
            "unexpected error: {error:#}"
        );
    }

    let inside_log = shared.child("inside.log");
    inside_log.write_str("existing log").unwrap();
    let outside_log_link = alias_dir.child("outside-log-link");
    std::os::unix::fs::symlink(inside_log.path(), outside_log_link.path()).unwrap();
    for log_path in [
        shared.path().join("new.log"),
        shared_alias.path().join("new.log"),
        outside_log_link.path().to_path_buf(),
    ] {
        let error = Args {
            serve_path: shared.path().to_path_buf(),
            state_dir: Some(state_dir.path().to_path_buf()),
            auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
            log_file: Some(log_path),
            ..Args::default()
        }
        .validate()
        .expect_err("a log path resolving inside the shared root was accepted");
        assert!(
            error.to_string().contains("resolve into shared path"),
            "unexpected error: {error:#}"
        );
    }

    let hard_link = alias_dir.child("hard-linked-log");
    std::fs::hard_link(inside_log.path(), hard_link.path()).unwrap();
    let error = Args {
        serve_path: shared.path().to_path_buf(),
        state_dir: Some(state_dir.path().to_path_buf()),
        auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
        log_file: Some(hard_link.path().to_path_buf()),
        ..Args::default()
    }
    .validate()
    .expect_err("a hard-linked log alias of a shared object was accepted");
    assert!(error.to_string().contains("exactly one hard link"));
}

#[test]
fn configuration_and_log_paths_must_have_distinct_identities() {
    let shared = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let config_dir = assert_fs::TempDir::new().unwrap();
    let config = config_dir.child("config.yaml");

    for log_path in [
        config.path().to_path_buf(),
        config_dir.path().join("config-log-alias"),
    ] {
        if log_path.as_path() != config.path() {
            std::os::unix::fs::symlink(config.path(), &log_path).unwrap();
        }
        config
            .write_str(&format!(
                "auth:\n  - {TEST_ACCOUNT}\nstate-dir: '{}'\nlog-file: '{}'\n",
                state_dir.path().display(),
                log_path.display()
            ))
            .unwrap();
        make_config_private(config.path());
        let matches = build_cli()
            .try_get_matches_from([
                OsString::from("dufs"),
                shared.path().as_os_str().to_owned(),
                OsString::from("--config"),
                config.path().as_os_str().to_owned(),
            ])
            .unwrap();
        let error = Args::parse(matches)
            .expect_err("configuration and log identity collision was accepted");
        assert!(
            error
                .to_string()
                .contains("conflicts by entry or object identity with log file"),
            "unexpected error: {error:#}"
        );
    }
}

#[test]
fn state_database_sidecar_conflicts_include_entry_and_object_aliases() {
    let shared = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let alias_dir = assert_fs::TempDir::new().unwrap();
    let state_alias = alias_dir.child("state-alias");
    std::os::unix::fs::symlink(state_dir.path(), state_alias.path()).unwrap();

    let aliased_entry = state_alias.path().join("state.sqlite3-wal");
    let error = Args {
        serve_path: shared.path().to_path_buf(),
        state_dir: Some(state_dir.path().to_path_buf()),
        auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
        log_file: Some(aliased_entry),
        ..Args::default()
    }
    .validate()
    .expect_err("an aliased SQLite sidecar entry was accepted as the log path");
    assert!(
        error
            .to_string()
            .contains("conflicts with SQLite state database")
    );

    let sidecar = state_dir.path().join("state.sqlite3-shm");
    std::fs::write(&sidecar, "existing sidecar").unwrap();
    std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600)).unwrap();
    let object_alias = alias_dir.child("sidecar-object-alias");
    std::os::unix::fs::symlink(&sidecar, object_alias.path()).unwrap();
    let error = Args {
        serve_path: shared.path().to_path_buf(),
        state_dir: Some(state_dir.path().to_path_buf()),
        auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
        log_file: Some(object_alias.path().to_path_buf()),
        ..Args::default()
    }
    .validate()
    .expect_err("an aliased SQLite sidecar object was accepted as the log path");
    assert!(
        error
            .to_string()
            .contains("conflicts with SQLite state database")
    );
}

#[test]
fn state_directory_rejects_a_renameable_ancestor_chain() {
    let shared = assert_fs::TempDir::new().unwrap();
    let container = assert_fs::TempDir::new().unwrap();
    let writable_parent = container.child("writable-parent");
    std::fs::create_dir(writable_parent.path()).unwrap();
    std::fs::set_permissions(
        writable_parent.path(),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let state_dir = writable_parent.child("state");
    std::fs::create_dir(state_dir.path()).unwrap();
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    let validate = || {
        Args {
            serve_path: shared.path().to_path_buf(),
            state_dir: Some(state_dir.path().to_path_buf()),
            auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
            ..Args::default()
        }
        .validate()
    };
    let error = validate().expect_err("a state directory under a renameable parent was accepted");
    assert!(
        error
            .to_string()
            .contains("can be renamed by untrusted local users"),
        "unexpected error: {error:#}"
    );

    std::fs::set_permissions(
        writable_parent.path(),
        std::fs::Permissions::from_mode(0o1777),
    )
    .unwrap();
    validate().expect("a sticky parent protecting a service-owned child was rejected");
}

#[test]
fn direct_validation_enforces_runtime_invariants() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let args = Args {
        serve_path: tmpdir.path().to_path_buf(),
        max_concurrent_uploads: 0,
        auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
        ..Args::default()
    };
    let error = args
        .validate()
        .expect_err("library configuration bypassed validation");
    assert!(error.to_string().contains("max-concurrent-uploads"));
}

#[test]
fn validated_config_exposes_only_a_normalized_read_view() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let relative = tmpdir.path().join(".");
    let args = Args {
        serve_path: relative,
        state_dir: Some(state_dir.path().to_path_buf()),
        auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
        ..Args::default()
    };
    let config = ValidatedConfig::try_from(args).unwrap();
    assert!(config.serve_path.is_absolute());
    assert!(config.serve_path.is_dir());
    assert!(config.auth.has_users());
}

#[test]
fn config_without_bind_uses_the_loopback_default() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let config_file = tmpdir.child("config.yaml");
    config_file
        .write_str(&format!("auth:\n  - {TEST_ACCOUNT}\n"))
        .unwrap();
    make_config_private(config_file.path());

    let matches = matches_with_state(["", "-c", &config_file.to_string_lossy()], state_dir.path());
    let args = Args::parse(matches).unwrap();
    assert_eq!(args.addrs, vec![IpAddr::from([127, 0, 0, 1])]);
}

#[test]
fn test_args_from_config_file1() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let config_dir = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let config_file = config_dir.child("config.yaml");
    let contents = format!(
        r#"
serve-path: {}
bind: 0.0.0.0
port: 3000
auth:
  - {TEST_ACCOUNT}
"#,
        yaml_path_scalar(tmpdir.path())
    );
    config_file.write_str(&contents).unwrap();
    make_config_private(config_file.path());

    let matches = matches_with_state(["", "-c", &config_file.to_string_lossy()], state_dir.path());
    let args = Args::parse(matches).unwrap();
    assert_eq!(args.serve_path, Args::sanitize_path(&tmpdir).unwrap());
    assert_eq!(args.addrs, vec!["0.0.0.0".parse::<IpAddr>().unwrap()]);
    assert_eq!(args.port, 3000);
}

#[test]
fn test_file_serve_path_from_config_is_rejected() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let shared_file = tmpdir.child("shared.txt");
    shared_file.write_str("contents").unwrap();
    let config_file = tmpdir.child("config.yaml");
    let contents = format!(
        r#"
serve-path: {}
auth:
  - {TEST_ACCOUNT}
"#,
        yaml_path_scalar(shared_file.path())
    );
    config_file.write_str(&contents).unwrap();
    make_config_private(config_file.path());

    let cli = build_cli();
    let matches = cli
        .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
        .unwrap();
    let err = Args::parse(matches).unwrap_err();
    assert!(
        err.to_string().contains("must be a directory"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn test_args_from_config_file2() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let config_file = tmpdir.child("config.yaml");
    let contents = format!(
        r#"
bind:
  - 127.0.0.1
  - 192.168.8.10
auth:
  - {TEST_ACCOUNT}
"#
    );
    config_file.write_str(&contents).unwrap();
    make_config_private(config_file.path());

    let matches = matches_with_state(["", "-c", &config_file.to_string_lossy()], state_dir.path());
    let args = Args::parse(matches).unwrap();
    assert_eq!(
        args.addrs,
        vec![
            "127.0.0.1".parse::<IpAddr>().unwrap(),
            "192.168.8.10".parse::<IpAddr>().unwrap()
        ]
    );
}

#[test]
fn empty_bind_list_is_rejected() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let config_file = tmpdir.child("config.yaml");
    config_file
        .write_str(&format!("bind: []\nauth:\n  - {TEST_ACCOUNT}\n"))
        .unwrap();
    make_config_private(config_file.path());

    let matches = build_cli()
        .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
        .unwrap();
    let error = Args::parse(matches).expect_err("an empty bind list was accepted");
    assert!(error.to_string().contains("bind must contain at least one"));
}

#[test]
fn test_non_ip_bind_config_is_rejected() {
    let tmpdir = assert_fs::TempDir::new().unwrap();
    let config_file = tmpdir.child("config.yaml");
    let contents = format!(
        r#"
serve-path: {}
bind: not-an-ip-address
auth:
  - {TEST_ACCOUNT}
"#,
        tmpdir.display()
    );
    config_file.write_str(&contents).unwrap();
    make_config_private(config_file.path());

    let cli = build_cli();
    let matches = cli
        .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
        .unwrap();
    let err = Args::parse(matches).unwrap_err();
    assert!(format!("{err:#}").contains("expected an IP address"));
}

#[test]
fn upload_limits_from_cli_override_defaults() {
    let state_dir = private_state_dir();
    let matches = matches_with_state(
        [
            "",
            "--max-upload-size",
            "0",
            "--upload-idle-timeout",
            "12",
            "--upload-total-timeout",
            "34",
            "--max-concurrent-uploads",
            "2",
            "--min-free-space",
            "0",
            "--max-connections",
            "11",
            "--max-search-entries",
            "12",
            "--max-concurrent-searches",
            "3",
            "--request-timeout",
            "16",
        ],
        state_dir.path(),
    );
    let args = Args::parse(matches).unwrap();
    assert_eq!(args.max_upload_size, 0);
    assert_eq!(args.upload_idle_timeout, 12);
    assert_eq!(args.upload_total_timeout, 34);
    assert_eq!(args.max_concurrent_uploads, 2);
    assert_eq!(args.min_free_space, 0);
    assert_eq!(args.max_connections, 11);
    assert_eq!(args.max_search_entries, 12);
    assert_eq!(args.max_concurrent_searches, 3);
    assert_eq!(args.request_timeout, 16);
}

#[test]
fn invalid_upload_time_and_concurrency_limits_are_rejected() {
    for invalid_args in [
        ["--upload-idle-timeout", "0"],
        ["--upload-total-timeout", "0"],
        ["--max-concurrent-uploads", "0"],
        ["--upload-idle-timeout", "60"],
    ] {
        let state_dir = private_state_dir();
        let mut values = vec![""];
        values.extend(invalid_args);
        if invalid_args[0] == "--upload-idle-timeout" && invalid_args[1] == "60" {
            values.extend(["--upload-total-timeout", "59"]);
        }
        let matches = matches_with_state(values, state_dir.path());
        assert!(Args::parse(matches).is_err(), "accepted {invalid_args:?}");
    }
}

#[test]
fn extreme_timeout_values_are_rejected_during_startup_validation() {
    for name in [
        "--upload-idle-timeout",
        "--upload-total-timeout",
        "--request-timeout",
    ] {
        for value in [(MAX_TIMEOUT_SECONDS + 1).to_string(), u64::MAX.to_string()] {
            let state_dir = private_state_dir();
            let mut values = vec!["".to_string(), name.to_string(), value];
            if name == "--upload-idle-timeout" {
                values.extend([
                    "--upload-total-timeout".to_string(),
                    MAX_TIMEOUT_SECONDS.to_string(),
                ]);
            }
            let matches = matches_with_state(values, state_dir.path());
            let error = Args::parse(matches).expect_err("extreme timeout value was accepted");
            assert!(
                error.to_string().contains("must not exceed"),
                "unexpected error for {name}: {error:#}"
            );
        }
    }
}

#[test]
fn maximum_timeout_value_is_accepted() {
    let state_dir = private_state_dir();
    let maximum = MAX_TIMEOUT_SECONDS.to_string();
    let matches = matches_with_state(
        [
            "",
            "--upload-idle-timeout",
            maximum.as_str(),
            "--upload-total-timeout",
            maximum.as_str(),
            "--request-timeout",
            maximum.as_str(),
        ],
        state_dir.path(),
    );
    let args = Args::parse(matches).unwrap();
    assert_eq!(args.upload_idle_timeout, MAX_TIMEOUT_SECONDS);
    assert_eq!(args.upload_total_timeout, MAX_TIMEOUT_SECONDS);
    assert_eq!(args.request_timeout, MAX_TIMEOUT_SECONDS);
}

#[test]
fn zero_ordinary_request_budgets_are_rejected() {
    for name in [
        "--max-connections",
        "--max-search-entries",
        "--max-concurrent-searches",
        "--request-timeout",
    ] {
        let state_dir = private_state_dir();
        let matches = matches_with_state(["", name, "0"], state_dir.path());
        assert!(Args::parse(matches).is_err(), "accepted {name}=0");
    }
}

#[test]
fn search_entry_limit_has_a_hard_upper_bound() {
    let state_dir = private_state_dir();
    for value in [MAX_SEARCH_ENTRIES, MAX_SEARCH_ENTRIES + 1] {
        let value_string = value.to_string();
        let matches = matches_with_state(
            ["", "--max-search-entries", value_string.as_str()],
            state_dir.path(),
        );
        let result = Args::parse(matches);
        if value == MAX_SEARCH_ENTRIES {
            assert!(result.is_ok());
        } else {
            assert!(
                result
                    .expect_err("an excessive search entry limit was accepted")
                    .to_string()
                    .contains("must not exceed")
            );
        }
    }
}

#[test]
fn semaphore_limits_above_tokios_maximum_are_rejected() {
    let too_many = (tokio::sync::Semaphore::MAX_PERMITS + 1).to_string();
    for name in [
        "--max-connections",
        "--max-concurrent-uploads",
        "--max-concurrent-searches",
    ] {
        let state_dir = private_state_dir();
        let matches = matches_with_state(["", name, too_many.as_str()], state_dir.path());
        let error = Args::parse(matches).expect_err("oversized semaphore count was accepted");
        assert!(
            error.to_string().contains("must not exceed"),
            "unexpected error for {name}: {error:#}"
        );
    }
}
