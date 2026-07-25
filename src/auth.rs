use anyhow::{Context, Result, anyhow, bail};
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use headers::HeaderValue;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

pub const SESSION_COOKIE_NAME: &str = "__Host-dufs-session";
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const SESSION_ABSOLUTE_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);
pub(crate) const MAX_USERNAME_BYTES: usize = 128;

const SESSION_CAPACITY: usize = 1024;
const SECRET_BYTES: usize = 32;
const SALT_BYTES: usize = 16;
const ARGON2_VERSION: u32 = 19;
const ARGON2_MEMORY_KIB: u32 = 19 * 1024;
const ARGON2_ITERATIONS: u32 = 2;
const ARGON2_PARALLELISM: u32 = 1;
const ARGON2_OUTPUT_BYTES: usize = 32;
const ENCODED_SECRET_LEN: usize = SECRET_BYTES * 2;
const COOKIE_ATTRIBUTES: &str = "Path=/; HttpOnly; Secure; SameSite=Strict";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub user: String,
    pub csrf_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub token: String,
    pub session: SessionInfo,
}

/// Proof that [`AccessControl`] has verified an account password.
///
/// The username is intentionally private and this type is not cloneable, so a
/// session can only be created from a successful verification result.
pub(crate) struct VerifiedUser {
    user: String,
}

#[derive(Debug)]
struct SessionRecord {
    user: String,
    csrf_token: [u8; SECRET_BYTES],
    created_at: Instant,
    last_seen: Instant,
}

impl SessionRecord {
    fn is_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created_at) >= SESSION_ABSOLUTE_TIMEOUT
            || now.saturating_duration_since(self.last_seen) >= SESSION_IDLE_TIMEOUT
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            user: self.user.clone(),
            csrf_token: hex::encode(self.csrf_token),
        }
    }
}

#[derive(Debug, Default)]
struct SessionStore {
    entries: HashMap<[u8; SECRET_BYTES], SessionRecord>,
}

impl SessionStore {
    fn purge_expired(&mut self, now: Instant) {
        self.entries.retain(|_, session| !session.is_expired(now));
    }

    fn insert(&mut self, token_digest: [u8; SECRET_BYTES], session: SessionRecord) -> Result<()> {
        if self.entries.contains_key(&token_digest) {
            bail!("Generated a duplicate session token");
        }

        if self.entries.len() >= SESSION_CAPACITY
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, session)| session.last_seen)
                .map(|(digest, _)| *digest)
        {
            self.entries.remove(&oldest);
        }

        self.entries.insert(token_digest, session);
        Ok(())
    }
}

#[derive(Clone)]
pub struct AccessControl {
    users: HashMap<String, String>,
    sessions: Arc<Mutex<SessionStore>>,
}

impl Default for AccessControl {
    fn default() -> Self {
        Self {
            users: HashMap::new(),
            sessions: Arc::new(Mutex::new(SessionStore::default())),
        }
    }
}

impl fmt::Debug for AccessControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut users: Vec<_> = self.users.keys().collect();
        users.sort_unstable();
        f.debug_struct("AccessControl")
            .field("users", &users)
            .field("active_sessions", &self.lock_sessions().entries.len())
            .finish()
    }
}

impl PartialEq for AccessControl {
    fn eq(&self, other: &Self) -> bool {
        self.users == other.users
    }
}

impl AccessControl {
    pub fn new(raw_accounts: &[&str]) -> Result<Self> {
        let mut users = HashMap::new();

        for (index, account) in raw_accounts.iter().enumerate() {
            let account_number = index + 1;
            let (user, password) = account.split_once(':').ok_or_else(|| {
                anyhow!("Invalid auth account #{account_number}: expected `user:<argon2id PHC>`")
            })?;
            if user.is_empty() || password.is_empty() {
                bail!(
                    "Invalid auth account #{account_number}: username and Argon2id PHC must not be empty"
                );
            }
            if user.len() > MAX_USERNAME_BYTES {
                bail!(
                    "Invalid auth account #{account_number}: username exceeds the {MAX_USERNAME_BYTES}-byte limit"
                );
            }
            if users.contains_key(user) {
                bail!("Invalid auth account #{account_number}: duplicate username");
            }

            validate_argon2id_hash(password).with_context(|| {
                format!("Invalid Argon2id PHC in auth account #{account_number}")
            })?;

            users.insert(user.to_string(), password.to_string());
        }

        Ok(Self {
            users,
            sessions: Arc::new(Mutex::new(SessionStore::default())),
        })
    }

    pub fn has_users(&self) -> bool {
        !self.users.is_empty()
    }

    /// Verify credentials without mutating the session store.
    pub(crate) fn verify_credentials(&self, user: &str, password: &str) -> Option<VerifiedUser> {
        self.verify_password(user, password).then(|| VerifiedUser {
            user: user.to_string(),
        })
    }

    /// Create a session for a previously verified account.
    pub(crate) fn create_session(
        &self,
        verified_user: VerifiedUser,
        previous_token: Option<&str>,
    ) -> Result<CreatedSession> {
        let user = verified_user.user;
        let token = random_secret()?;
        let token_digest = session_digest(&token);
        let csrf_token = random_bytes()?;
        let now = Instant::now();

        let mut sessions = self.lock_sessions();
        sessions.purge_expired(now);
        if sessions.entries.contains_key(&token_digest) {
            bail!("Generated a duplicate session token");
        }
        // Check the new digest before removing the previous session, then
        // replace under the same lock. This preserves the old session on a
        // random-token collision and avoids evicting an unrelated session
        // when a full store rotates one of its existing entries.
        if let Some(previous_token) = previous_token {
            sessions.entries.remove(&session_digest(previous_token));
        }
        sessions.insert(
            token_digest,
            SessionRecord {
                user: user.clone(),
                csrf_token,
                created_at: now,
                last_seen: now,
            },
        )?;

        Ok(CreatedSession {
            token,
            session: SessionInfo {
                user,
                csrf_token: hex::encode(csrf_token),
            },
        })
    }

    /// Verify credentials and create a fresh session.
    ///
    /// If `previous_token` is present, it is removed only after the
    /// credentials have been verified and a replacement token has been
    /// generated successfully.
    #[cfg(test)]
    pub fn login(
        &self,
        user: &str,
        password: &str,
        previous_token: Option<&str>,
    ) -> Result<Option<CreatedSession>> {
        let Some(verified_user) = self.verify_credentials(user, password) else {
            return Ok(None);
        };
        self.create_session(verified_user, previous_token).map(Some)
    }

    /// Authenticate a session and refresh its idle timeout.
    pub fn authenticate(&self, token: &str) -> Option<SessionInfo> {
        self.authenticate_at(token, Instant::now())
    }

    /// Remove a session. Returns whether an active or expired record existed.
    pub fn logout(&self, token: &str) -> bool {
        if !is_encoded_secret(token) {
            return false;
        }
        self.lock_sessions()
            .entries
            .remove(&session_digest(token))
            .is_some()
    }

    /// Check the CSRF value belonging to an unexpired session.
    ///
    /// The comparison always covers all 256 bits for a syntactically valid
    /// or invalid candidate of the expected length.
    pub fn verify_csrf(&self, token: &str, expected: &str, candidate: &str) -> bool {
        if !is_encoded_secret(token) {
            return false;
        }

        let now = Instant::now();
        let digest = session_digest(token);
        let mut sessions = self.lock_sessions();
        let Some(session) = sessions.entries.get(&digest) else {
            return false;
        };
        if session.is_expired(now) {
            sessions.entries.remove(&digest);
            return false;
        }

        let (expected, expected_valid) = decode_secret_for_comparison(expected);
        let (candidate, candidate_valid) = decode_secret_for_comparison(candidate);
        let expected_equal = session.csrf_token.ct_eq(&expected);
        let candidate_equal = session.csrf_token.ct_eq(&candidate);
        bool::from(expected_equal & candidate_equal) && expected_valid && candidate_valid
    }

    fn verify_password(&self, user: &str, password: &str) -> bool {
        let Some((password_hash, known_user)) = self
            .users
            .get(user)
            .map(|hash| (hash, true))
            .or_else(|| self.users.values().next().map(|hash| (hash, false)))
        else {
            return false;
        };

        let verified = PasswordHash::new(password_hash).ok().is_some_and(|hash| {
            current_argon2()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        known_user && verified
    }

    fn authenticate_at(&self, token: &str, now: Instant) -> Option<SessionInfo> {
        if !is_encoded_secret(token) {
            return None;
        }

        let digest = session_digest(token);
        let mut sessions = self.lock_sessions();
        let expired = sessions
            .entries
            .get(&digest)
            .is_some_and(|session| session.is_expired(now));
        if expired {
            sessions.entries.remove(&digest);
            return None;
        }

        let session = sessions.entries.get_mut(&digest)?;
        session.last_seen = now;
        Some(session.info())
    }

    fn lock_sessions(&self) -> MutexGuard<'_, SessionStore> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn session_cookie(&self, token: &str) -> Result<HeaderValue> {
        session_cookie(token)
    }

    pub fn clear_session_cookie(&self) -> HeaderValue {
        clear_session_cookie()
    }
}

/// Hash a password with Argon2id v19 and the crate's standard parameters.
pub fn hash_password(password: &str) -> Result<String> {
    let mut salt = [0u8; SALT_BYTES];
    getrandom::fill(&mut salt).map_err(|err| anyhow!("Failed to generate password salt: {err}"))?;
    let salt = SaltString::encode_b64(&salt)
        .map_err(|err| anyhow!("Failed to encode password salt: {err}"))?;
    current_argon2()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| anyhow!("Failed to hash password: {err}"))
}

pub fn session_cookie(token: &str) -> Result<HeaderValue> {
    if !is_encoded_secret(token) {
        bail!("Invalid session token");
    }
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE_NAME}={token}; {COOKIE_ATTRIBUTES}"
    ))
    .context("Failed to create the session cookie")
}

pub fn clear_session_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "__Host-dufs-session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
    )
}

pub fn session_token_from_cookie(cookie: &HeaderValue) -> Option<&str> {
    cookie
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| {
            (name == SESSION_COOKIE_NAME && is_encoded_secret(value)).then_some(value)
        })
}

fn validate_argon2id_hash(password_hash: &str) -> Result<()> {
    let parsed =
        PasswordHash::new(password_hash).map_err(|err| anyhow!("Invalid PHC string: {err}"))?;
    if parsed.algorithm.as_str() != "argon2id" {
        bail!("Expected the argon2id algorithm");
    }
    if parsed.version != Some(ARGON2_VERSION) {
        bail!("Expected Argon2id version 19");
    }
    let Some(salt) = parsed.salt else {
        bail!("Argon2id PHC string must include a salt and output");
    };
    let Some(output) = parsed.hash else {
        bail!("Argon2id PHC string must include a salt and output");
    };

    let mut decoded_salt = [0u8; 64];
    let decoded_salt = salt
        .decode_b64(&mut decoded_salt)
        .map_err(|err| anyhow!("Invalid Argon2id salt: {err}"))?;
    if decoded_salt.len() != SALT_BYTES {
        bail!("Argon2id salt must be exactly {SALT_BYTES} bytes");
    }
    if output.len() != ARGON2_OUTPUT_BYTES {
        bail!("Argon2id output must be exactly {ARGON2_OUTPUT_BYTES} bytes");
    }

    if parsed.params.iter().count() != 3
        || parsed.params.get_decimal("m") != Some(ARGON2_MEMORY_KIB)
        || parsed.params.get_decimal("t") != Some(ARGON2_ITERATIONS)
        || parsed.params.get_decimal("p") != Some(ARGON2_PARALLELISM)
    {
        bail!(
            "Argon2id parameters must be exactly m={},t={},p={}",
            ARGON2_MEMORY_KIB,
            ARGON2_ITERATIONS,
            ARGON2_PARALLELISM
        );
    }

    let params =
        Params::try_from(&parsed).map_err(|err| anyhow!("Invalid Argon2id parameters: {err}"))?;
    if params.m_cost() != ARGON2_MEMORY_KIB
        || params.t_cost() != ARGON2_ITERATIONS
        || params.p_cost() != ARGON2_PARALLELISM
        || params.output_len() != Some(ARGON2_OUTPUT_BYTES)
        || !params.keyid().is_empty()
        || !params.data().is_empty()
    {
        bail!("Argon2id PHC string does not match the current password policy");
    }
    Ok(())
}

fn current_argon2() -> Argon2<'static> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(ARGON2_OUTPUT_BYTES),
    )
    .expect("the fixed Argon2id password policy is valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn random_bytes() -> Result<[u8; SECRET_BYTES]> {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|err| anyhow!("Failed to generate random secret: {err}"))?;
    Ok(bytes)
}

fn random_secret() -> Result<String> {
    Ok(hex::encode(random_bytes()?))
}

fn session_digest(token: &str) -> [u8; SECRET_BYTES] {
    Sha256::digest(token.as_bytes()).into()
}

fn is_encoded_secret(value: &str) -> bool {
    value.len() == ENCODED_SECRET_LEN && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn decode_secret_for_comparison(value: &str) -> ([u8; SECRET_BYTES], bool) {
    let mut decoded = [0u8; SECRET_BYTES];
    let valid =
        value.len() == ENCODED_SECRET_LEN && hex::decode_to_slice(value, &mut decoded).is_ok();
    (decoded, valid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_access_control() -> AccessControl {
        let hash = hash_password("pass").unwrap();
        let account = format!("user:{hash}");
        AccessControl::new(&[&account]).unwrap()
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
    fn configured_argon2id_phc_is_accepted() {
        let hash = hash_password("secret").unwrap();
        let account = format!("user:{hash}");
        let auth = AccessControl::new(&[&account]).unwrap();
        assert!(auth.login("user", "secret", None).unwrap().is_some());
        assert_eq!(auth.users.get("user"), Some(&hash));
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
            let error = validate_argon2id_hash(&invalid)
                .expect_err("non-current Argon2id policy was accepted");
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
            let err = AccessControl::new(&[account]).unwrap_err().to_string();
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
            AccessControl::new(&[&first, &second])
                .unwrap_err()
                .to_string()
                .contains("duplicate username")
        );
        assert!(
            AccessControl::new(&["user:"])
                .unwrap_err()
                .to_string()
                .contains("must not be empty")
        );
        assert!(!AccessControl::default().has_users());
    }

    #[test]
    fn configured_username_uses_the_browser_login_byte_limit_without_echoing_it() {
        let hash = hash_password("secret").unwrap();
        let maximum_username = "u".repeat(MAX_USERNAME_BYTES);
        let maximum_account = format!("{maximum_username}:{hash}");
        assert!(AccessControl::new(&[&maximum_account]).is_ok());

        let oversized_username = "private-user".repeat(MAX_USERNAME_BYTES);
        let oversized_account = format!("{oversized_username}:{hash}");
        let error = AccessControl::new(&[&oversized_account])
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
        let auth = test_access_control();
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
            let now = Instant::now();
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
                            user: "user".to_string(),
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
    fn authentication_refreshes_idle_time_and_enforces_both_expirations() {
        let auth = test_access_control();
        let idle = auth.login("user", "pass", None).unwrap().unwrap();
        let absolute = auth.login("user", "pass", None).unwrap().unwrap();
        let now = Instant::now();

        {
            let mut sessions = auth.lock_sessions();
            let idle_record = sessions
                .entries
                .get_mut(&session_digest(&idle.token))
                .unwrap();
            idle_record.last_seen = now - SESSION_IDLE_TIMEOUT - Duration::from_secs(1);

            let absolute_record = sessions
                .entries
                .get_mut(&session_digest(&absolute.token))
                .unwrap();
            absolute_record.created_at = now - SESSION_ABSOLUTE_TIMEOUT - Duration::from_secs(1);
            absolute_record.last_seen = now;
        }

        assert!(auth.authenticate_at(&idle.token, now).is_none());
        assert!(auth.authenticate_at(&absolute.token, now).is_none());

        let active = auth.login("user", "pass", None).unwrap().unwrap();
        let before = {
            let mut sessions = auth.lock_sessions();
            let record = sessions
                .entries
                .get_mut(&session_digest(&active.token))
                .unwrap();
            record.last_seen = now - Duration::from_secs(60);
            record.last_seen
        };
        assert!(auth.authenticate_at(&active.token, now).is_some());
        let after = auth
            .lock_sessions()
            .entries
            .get(&session_digest(&active.token))
            .unwrap()
            .last_seen;
        assert!(after > before);
    }

    #[test]
    fn session_store_is_bounded_and_evicts_the_least_recent_session() {
        let mut store = SessionStore::default();
        let now = Instant::now();
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
                        user: "user".to_string(),
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
}
