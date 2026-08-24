use super::*;

use assert_fs::prelude::*;
use std::ffi::OsStr;
use std::os::fd::AsRawFd;

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
    values.push("--state-dir".into());
    values.push(state_dir.as_os_str().to_os_string());
    build_cli().try_get_matches_from(values).unwrap()
}

#[test]
fn test_default() {
    let state_dir = private_state_dir();
    let matches = matches_with_state(["", "--auth", TEST_ACCOUNT], state_dir.path());
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
            "--auth",
            TEST_ACCOUNT,
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
        let matches = matches_with_state(
            ["", "--auth", TEST_ACCOUNT, "--trusted-proxy", unbounded],
            state_dir.path(),
        );
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
    let state_dir = private_state_dir();
    let config_file = tmpdir.child("config.yaml");
    let contents = format!(
        r#"
serve-path: {}
bind: 0.0.0.0
port: 3000
auth:
  - {TEST_ACCOUNT}
"#,
        tmpdir.display()
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
        shared_file.display()
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
            "--auth",
            TEST_ACCOUNT,
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
        let mut values = vec!["", "--auth", TEST_ACCOUNT];
        values.extend(invalid_args);
        if invalid_args[0] == "--upload-idle-timeout" && invalid_args[1] == "60" {
            values.extend(["--upload-total-timeout", "59"]);
        }
        let matches = build_cli().try_get_matches_from(values).unwrap();
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
            let mut values = vec![
                "".to_string(),
                "--auth".to_string(),
                TEST_ACCOUNT.to_string(),
                name.to_string(),
                value,
            ];
            if name == "--upload-idle-timeout" {
                values.extend([
                    "--upload-total-timeout".to_string(),
                    MAX_TIMEOUT_SECONDS.to_string(),
                ]);
            }
            let matches = build_cli().try_get_matches_from(values).unwrap();
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
            "--auth",
            TEST_ACCOUNT,
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
        let matches = build_cli()
            .try_get_matches_from(["", "--auth", TEST_ACCOUNT, name, "0"])
            .unwrap();
        assert!(Args::parse(matches).is_err(), "accepted {name}=0");
    }
}

#[test]
fn search_entry_limit_has_a_hard_upper_bound() {
    let state_dir = private_state_dir();
    for value in [MAX_SEARCH_ENTRIES, MAX_SEARCH_ENTRIES + 1] {
        let value_string = value.to_string();
        let matches = matches_with_state(
            [
                "",
                "--auth",
                TEST_ACCOUNT,
                "--max-search-entries",
                value_string.as_str(),
            ],
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
        let matches = build_cli()
            .try_get_matches_from(["", "--auth", TEST_ACCOUNT, name, too_many.as_str()])
            .unwrap();
        let error = Args::parse(matches).expect_err("oversized semaphore count was accepted");
        assert!(
            error.to_string().contains("must not exceed"),
            "unexpected error for {name}: {error:#}"
        );
    }
}
