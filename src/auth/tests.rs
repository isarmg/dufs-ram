use super::*;

fn test_access_control() -> AccessControl {
    test_access_control_with_clock(SessionClock::boottime())
}

fn test_access_control_with_clock(clock: SessionClock) -> AccessControl {
    let hash = hash_password("pass").unwrap();
    let account = format!("user:{hash}");
    AccessControl::with_clock(AuthConfig::new(&[&account]).unwrap(), clock)
}

fn session_test_time() -> SessionInstant {
    SessionInstant(Duration::from_secs(24 * 60 * 60))
}

#[derive(Clone)]
struct ManualSessionClock {
    current: Arc<Mutex<SessionInstant>>,
}

impl ManualSessionClock {
    fn new(current: SessionInstant) -> Self {
        Self {
            current: Arc::new(Mutex::new(current)),
        }
    }

    fn session_clock(&self) -> SessionClock {
        let current = Arc::clone(&self.current);
        SessionClock::injected(move || {
            *current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
    }

    fn set(&self, current: SessionInstant) {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = current;
    }
}

#[test]
fn password_hashes_are_argon2id_and_salted() {
    let first = hash_password("correct horse battery staple").unwrap();
    let second = hash_password("correct horse battery staple").unwrap();
    assert!(first.starts_with("$argon2id$v=19$"));
    assert!(second.starts_with("$argon2id$v=19$"));
    assert_ne!(first, second);

    let parsed = PasswordHash::new(&first).unwrap();
    assert!(
        Argon2::default()
            .verify_password(b"correct horse battery staple", &parsed)
            .is_ok()
    );
    assert!(
        Argon2::default()
            .verify_password(b"wrong", &parsed)
            .is_err()
    );
}

#[test]
fn password_hashing_enforces_the_login_byte_limit() {
    let empty_error = hash_password("")
        .expect_err("an empty password was accepted")
        .to_string();
    assert!(
        empty_error.contains("must not be empty"),
        "unexpected error: {empty_error}"
    );
    for password in [
        "p".repeat(MAX_PASSWORD_BYTES),
        "é".repeat(MAX_PASSWORD_BYTES / "é".len()),
    ] {
        assert!(hash_password(&password).is_ok());
    }
    for password in [
        "p".repeat(MAX_PASSWORD_BYTES + 1),
        format!("a{}", "é".repeat(MAX_PASSWORD_BYTES / "é".len())),
    ] {
        let error = hash_password(&password)
            .expect_err("an oversized password was accepted")
            .to_string();
        assert!(
            error.contains("1024-byte limit"),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn configured_argon2id_phc_is_accepted() {
    let hash = hash_password("secret").unwrap();
    let account = format!("user:{hash}");
    let auth = AccessControl::from_config(AuthConfig::new(&[&account]).unwrap());
    assert!(auth.login("user", "secret", None).unwrap().is_some());
    assert_eq!(auth.config.users.get("user"), Some(&hash));
}

#[test]
fn auth_config_debug_never_exposes_password_hashes() {
    let hash = hash_password("debug-secret").unwrap();
    let account = format!("visible-user:{hash}");
    let config = AuthConfig::new(&[&account]).unwrap();

    let output = format!("{config:?}");
    assert!(output.contains("visible-user"));
    assert!(!output.contains("argon2id"));
    assert!(!output.contains(&hash));
}

#[test]
fn access_controls_from_cloned_config_have_isolated_sessions() {
    let hash = hash_password("pass").unwrap();
    let account = format!("user:{hash}");
    let config = AuthConfig::new(&[&account]).unwrap();
    let first = AccessControl::from_config(config.clone());
    let second = AccessControl::from_config(config);

    let created = first.login("user", "pass", None).unwrap().unwrap();
    assert!(first.authenticate(&created.token).is_some());
    assert!(second.authenticate(&created.token).is_none());
}

#[test]
fn configured_hash_must_match_the_current_argon2id_policy_exactly() {
    let valid = hash_password("secret").unwrap();
    validate_argon2id_hash(&valid).unwrap();

    let salt = SaltString::encode_b64(&[0x5a; SALT_BYTES]).unwrap();
    let short_output_params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(16),
    )
    .unwrap();
    let short_output = Argon2::new(Algorithm::Argon2id, Version::V0x13, short_output_params)
        .hash_password(b"secret", &salt)
        .unwrap()
        .to_string();

    let short_salt = SaltString::encode_b64(&[0x5a; 8]).unwrap();
    let short_salt = current_argon2()
        .hash_password(b"secret", &short_salt)
        .unwrap()
        .to_string();

    for invalid in [
        valid.replacen("v=19", "v=16", 1),
        valid.replacen("$v=19", "", 1),
        valid.replacen("m=19456", "m=8", 1),
        valid.replacen("t=2", "t=3", 1),
        valid.replacen("p=1", "p=2", 1),
        valid.replacen(",p=1", "", 1),
        short_output,
        short_salt,
    ] {
        let error =
            validate_argon2id_hash(&invalid).expect_err("non-current Argon2id policy was accepted");
        assert!(
            !error.to_string().contains(&invalid),
            "password hash was echoed in the validation error"
        );
    }
}

#[test]
fn invalid_password_configurations_are_rejected_without_echoing_secrets() {
    for account in [
        "missing-separator-secret",
        "user:not-a-valid-phc-secret",
        "user:$argon2id$malformed-secret",
    ] {
        let err = AuthConfig::new(&[account]).unwrap_err().to_string();
        assert!(err.contains("auth account #1"), "unexpected error: {err}");
        assert!(!err.contains(account), "error echoed account: {err}");
        assert!(!err.contains("secret"), "error echoed secret: {err}");
    }
}

#[test]
fn invalid_accounts_are_rejected() {
    let hash = hash_password("secret").unwrap();
    let first = format!("user:{hash}");
    let second = format!("user:{hash}");
    assert!(
        AuthConfig::new(&[&first, &second])
            .unwrap_err()
            .to_string()
            .contains("duplicate username")
    );
    assert!(
        AuthConfig::new(&["user:"])
            .unwrap_err()
            .to_string()
            .contains("must not be empty")
    );
    assert!(!AccessControl::default().has_users());
}

#[test]
fn oversized_account_lists_are_rejected_before_parsing_entries() {
    let accounts = vec!["invalid"; MAX_ACCOUNTS + 1];
    let error = AuthConfig::new(&accounts).expect_err("an oversized account list was accepted");
    assert!(error.to_string().contains("At most"));
}

#[test]
fn configured_username_uses_the_browser_login_byte_limit_without_echoing_it() {
    let hash = hash_password("secret").unwrap();
    let maximum_username = "u".repeat(MAX_USERNAME_BYTES);
    let maximum_account = format!("{maximum_username}:{hash}");
    assert!(AuthConfig::new(&[&maximum_account]).is_ok());

    let oversized_username = "private-user".repeat(MAX_USERNAME_BYTES);
    let oversized_account = format!("{oversized_username}:{hash}");
    let error = AuthConfig::new(&[&oversized_account])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("auth account #1"),
        "unexpected error: {error}"
    );
    assert!(
        error.contains("128-byte limit"),
        "unexpected error: {error}"
    );
    assert!(
        !error.contains(&oversized_username),
        "error echoed the configured username"
    );
}

#[test]
fn login_creates_an_opaque_digest_stored_session_and_rotates() {
    let auth = test_access_control();
    let first = auth.login("user", "pass", None).unwrap().unwrap();
    assert_eq!(first.session.user, "user");
    assert_eq!(first.token.len(), ENCODED_SECRET_LEN);
    assert_eq!(first.session.csrf_token.len(), ENCODED_SECRET_LEN);

    let digest = session_digest(&first.token);
    let sessions = auth.lock_sessions();
    assert!(sessions.entries.contains_key(&digest));
    assert_eq!(sessions.entries.len(), 1);
    drop(sessions);

    let info = auth.authenticate(&first.token).unwrap();
    assert_eq!(info.user, "user");
    assert_eq!(info.csrf_token, first.session.csrf_token);

    let second = auth
        .login("user", "pass", Some(&first.token))
        .unwrap()
        .unwrap();
    assert_ne!(first.token, second.token);
    assert!(auth.authenticate(&first.token).is_none());
    assert!(auth.authenticate(&second.token).is_some());
    assert_eq!(auth.lock_sessions().entries.len(), 1);
}

#[test]
fn rotating_a_full_session_store_does_not_evict_an_unrelated_session() {
    let now = session_test_time();
    let clock = ManualSessionClock::new(now);
    let auth = test_access_control_with_clock(clock.session_clock());
    let first = auth
        .create_session(
            VerifiedUser {
                user: "user".to_string(),
            },
            None,
        )
        .unwrap();

    {
        let mut sessions = auth.lock_sessions();
        let mut index = 0u64;
        while sessions.entries.len() < SESSION_CAPACITY {
            let digest: [u8; SECRET_BYTES] = Sha256::digest(index.to_be_bytes()).into();
            index += 1;
            if sessions.entries.contains_key(&digest) {
                continue;
            }
            sessions
                .insert(
                    digest,
                    SessionRecord {
                        user: format!("other-{index}"),
                        csrf_token: [0; SECRET_BYTES],
                        created_at: now,
                        last_seen: now,
                    },
                )
                .unwrap();
        }
    }

    let second = auth
        .create_session(
            VerifiedUser {
                user: "user".to_string(),
            },
            Some(&first.token),
        )
        .unwrap();
    let sessions = auth.lock_sessions();
    assert_eq!(sessions.entries.len(), SESSION_CAPACITY);
    assert!(!sessions.entries.contains_key(&session_digest(&first.token)));
    assert!(
        sessions
            .entries
            .contains_key(&session_digest(&second.token))
    );
}

#[test]
fn failed_login_does_not_revoke_an_existing_session() {
    let auth = test_access_control();
    let session = auth.login("user", "pass", None).unwrap().unwrap();
    assert!(
        auth.login("user", "wrong", Some(&session.token))
            .unwrap()
            .is_none()
    );
    assert!(auth.authenticate(&session.token).is_some());
}

#[test]
fn logout_revokes_the_session() {
    let auth = test_access_control();
    let session = auth.login("user", "pass", None).unwrap().unwrap();
    assert!(auth.logout(&session.token));
    assert!(!auth.logout(&session.token));
    assert!(auth.authenticate(&session.token).is_none());
}

#[test]
fn csrf_is_random_session_bound_and_compared_by_value() {
    let auth = test_access_control();
    let first = auth.login("user", "pass", None).unwrap().unwrap();
    let second = auth.login("user", "pass", None).unwrap().unwrap();
    assert_ne!(first.session.csrf_token, second.session.csrf_token);
    assert!(auth.verify_csrf(
        &first.token,
        &first.session.csrf_token,
        &first.session.csrf_token
    ));
    assert!(!auth.verify_csrf(
        &first.token,
        &first.session.csrf_token,
        &second.session.csrf_token
    ));
    assert!(!auth.verify_csrf(&first.token, &first.session.csrf_token, "invalid"));

    let mut tampered = first.session.csrf_token.clone().into_bytes();
    tampered[0] = if tampered[0] == b'0' { b'1' } else { b'0' };
    assert!(!auth.verify_csrf(
        &first.token,
        &first.session.csrf_token,
        std::str::from_utf8(&tampered).unwrap()
    ));
}

#[test]
fn injected_clock_enforces_idle_csrf_and_absolute_expiration_boundaries() {
    let start = session_test_time();
    let clock = ManualSessionClock::new(start);
    let auth = test_access_control_with_clock(clock.session_clock());
    let idle = auth.login("user", "pass", None).unwrap().unwrap();
    let csrf = auth.login("user", "pass", None).unwrap().unwrap();
    let absolute = auth.login("user", "pass", None).unwrap().unwrap();

    // Advancing the injected clock without executing application code models
    // elapsed CLOCK_BOOTTIME while the machine is suspended.
    clock.set(start + (SESSION_IDLE_TIMEOUT - Duration::from_nanos(1)));
    assert!(auth.verify_csrf(
        &csrf.token,
        &csrf.session.csrf_token,
        &csrf.session.csrf_token
    ));
    assert!(auth.authenticate(&absolute.token).is_some());
    assert_eq!(
        auth.lock_sessions()
            .entries
            .get(&session_digest(&absolute.token))
            .unwrap()
            .last_seen,
        start + (SESSION_IDLE_TIMEOUT - Duration::from_nanos(1))
    );

    clock.set(start + SESSION_IDLE_TIMEOUT);
    assert!(auth.authenticate(&idle.token).is_none());
    assert!(!auth.verify_csrf(
        &csrf.token,
        &csrf.session.csrf_token,
        &csrf.session.csrf_token
    ));
    assert!(auth.authenticate(&absolute.token).is_some());

    let refresh_step = Duration::from_secs(15 * 60);
    let mut elapsed = SESSION_IDLE_TIMEOUT + refresh_step;
    while elapsed < SESSION_ABSOLUTE_TIMEOUT {
        clock.set(start + elapsed);
        assert!(auth.authenticate(&absolute.token).is_some());
        elapsed += refresh_step;
    }

    clock.set(start + (SESSION_ABSOLUTE_TIMEOUT - Duration::from_nanos(1)));
    assert!(auth.authenticate(&absolute.token).is_some());
    clock.set(start + SESSION_ABSOLUTE_TIMEOUT);
    assert!(auth.authenticate(&absolute.token).is_none());
    assert!(
        !auth
            .lock_sessions()
            .entries
            .contains_key(&session_digest(&absolute.token))
    );
}

#[test]
fn injected_clock_regression_does_not_underflow_or_move_idle_time_backwards() {
    let start = session_test_time();
    let clock = ManualSessionClock::new(start);
    let auth = test_access_control_with_clock(clock.session_clock());
    let session = auth.login("user", "pass", None).unwrap().unwrap();

    clock.set(SessionInstant(start.0 - Duration::from_secs(1)));
    assert!(auth.authenticate(&session.token).is_some());
    let stored = auth.lock_sessions();
    let record = stored.entries.get(&session_digest(&session.token)).unwrap();
    assert_eq!(record.created_at, start);
    assert_eq!(record.last_seen, start);
}

#[test]
fn session_store_is_bounded_and_evicts_the_least_recent_session() {
    let mut store = SessionStore::default();
    let now = session_test_time();
    let oldest_digest = session_digest("oldest");

    store
        .insert(
            oldest_digest,
            SessionRecord {
                user: "user".to_string(),
                csrf_token: [0; SECRET_BYTES],
                created_at: now,
                last_seen: now,
            },
        )
        .unwrap();

    for index in 1..=SESSION_CAPACITY {
        let digest: [u8; SECRET_BYTES] = Sha256::digest(index.to_be_bytes()).into();
        store
            .insert(
                digest,
                SessionRecord {
                    user: format!("user-{index}"),
                    csrf_token: [0; SECRET_BYTES],
                    created_at: now,
                    last_seen: now + Duration::from_nanos(index as u64),
                },
            )
            .unwrap();
    }

    assert_eq!(store.entries.len(), SESSION_CAPACITY);
    assert!(!store.entries.contains_key(&oldest_digest));
}

#[test]
fn session_store_enforces_the_per_user_capacity_before_global_capacity() {
    let mut store = SessionStore::default();
    let now = session_test_time();
    let oldest = session_digest("same-user-0");
    for index in 0..=SESSION_PER_USER_CAPACITY {
        store
            .insert(
                session_digest(&format!("same-user-{index}")),
                SessionRecord {
                    user: "same-user".to_string(),
                    csrf_token: [0; SECRET_BYTES],
                    created_at: now,
                    last_seen: now + Duration::from_nanos(index as u64),
                },
            )
            .unwrap();
    }

    assert_eq!(store.entries.len(), SESSION_PER_USER_CAPACITY);
    assert!(!store.entries.contains_key(&oldest));
}

#[test]
fn repeated_logins_evict_the_same_users_oldest_session_first() {
    let mut store = SessionStore::default();
    let now = session_test_time();
    let protected_digest = session_digest("protected-oldest");
    store
        .insert(
            protected_digest,
            SessionRecord {
                user: "protected".to_string(),
                csrf_token: [0; SECRET_BYTES],
                created_at: now,
                last_seen: now,
            },
        )
        .unwrap();

    let attacker_oldest = session_digest("attacker-0");
    for index in 0..SESSION_PER_USER_CAPACITY {
        store
            .insert(
                session_digest(&format!("attacker-{index}")),
                SessionRecord {
                    user: "attacker".to_string(),
                    csrf_token: [0; SECRET_BYTES],
                    created_at: now + Duration::from_secs(1),
                    last_seen: now + Duration::from_secs(index as u64 + 1),
                },
            )
            .unwrap();
    }

    let mut filler = 0usize;
    while store.entries.len() < SESSION_CAPACITY {
        store
            .insert(
                session_digest(&format!("filler-{filler}")),
                SessionRecord {
                    user: format!("filler-{filler}"),
                    csrf_token: [0; SECRET_BYTES],
                    created_at: now + Duration::from_secs(100),
                    last_seen: now + Duration::from_secs(100),
                },
            )
            .unwrap();
        filler += 1;
    }

    let newest = session_digest("attacker-newest");
    store
        .insert(
            newest,
            SessionRecord {
                user: "attacker".to_string(),
                csrf_token: [0; SECRET_BYTES],
                created_at: now + Duration::from_secs(200),
                last_seen: now + Duration::from_secs(200),
            },
        )
        .unwrap();

    assert_eq!(store.entries.len(), SESSION_CAPACITY);
    assert!(
        store.entries.contains_key(&protected_digest),
        "another account's globally oldest session was evicted"
    );
    assert!(!store.entries.contains_key(&attacker_oldest));
    assert!(store.entries.contains_key(&newest));
    assert_eq!(
        store
            .entries
            .values()
            .filter(|session| session.user == "attacker")
            .count(),
        SESSION_PER_USER_CAPACITY
    );
}

#[test]
fn cookie_helpers_use_host_only_secure_attributes() {
    let token = "ab".repeat(SECRET_BYTES);
    let cookie = session_cookie(&token).unwrap();
    assert_eq!(
        cookie.to_str().unwrap(),
        format!("{SESSION_COOKIE_NAME}={token}; Path=/; HttpOnly; Secure; SameSite=Strict")
    );
    assert_eq!(session_token_from_cookie(&cookie), Some(token.as_str()));

    let request_cookie = HeaderValue::from_str(&format!(
        "theme=dark; {SESSION_COOKIE_NAME}={token}; language=zh"
    ))
    .unwrap();
    assert_eq!(
        session_token_from_cookie(&request_cookie),
        Some(token.as_str())
    );

    let clear_cookie = clear_session_cookie();
    let cleared = clear_cookie.to_str().unwrap();
    assert!(cleared.starts_with(&format!("{SESSION_COOKIE_NAME}=;")));
    for attribute in [
        "Path=/",
        "HttpOnly",
        "Secure",
        "SameSite=Strict",
        "Max-Age=0",
    ] {
        assert!(cleared.contains(attribute));
    }
    assert!(!cookie.to_str().unwrap().contains("Domain="));
    assert!(!cleared.contains("Domain="));
}

#[test]
fn cloned_access_control_shares_session_state() {
    let auth = test_access_control();
    let clone = auth.clone();
    let session = auth.login("user", "pass", None).unwrap().unwrap();
    assert!(clone.authenticate(&session.token).is_some());
    assert!(clone.logout(&session.token));
    assert!(auth.authenticate(&session.token).is_none());
}
