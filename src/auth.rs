use crate::utils::{decode_hex_to_slice, encode_hex};
use anyhow::{Context, Result, anyhow, bail};
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use headers::HeaderValue;
use serde::{Deserialize, Deserializer};
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
pub const MAX_PASSWORD_BYTES: usize = 1024;
pub(crate) const MAX_USERNAME_BYTES: usize = 128;

const SESSION_CAPACITY: usize = 1024;
const SESSION_PER_USER_CAPACITY: usize = 32;
const MAX_ACCOUNTS: usize = 1024;
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
            csrf_token: encode_hex(self.csrf_token),
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

        let same_user_count = self
            .entries
            .values()
            .filter(|existing| existing.user == session.user)
            .count();
        let oldest_for_user = || {
            self.entries
                .iter()
                .filter(|(_, existing)| existing.user == session.user)
                .min_by_key(|(_, existing)| (existing.last_seen, existing.created_at))
                .map(|(digest, _)| *digest)
        };
        let oldest = if same_user_count >= SESSION_PER_USER_CAPACITY
            || (self.entries.len() >= SESSION_CAPACITY && same_user_count > 0)
        {
            oldest_for_user()
        } else if self.entries.len() >= SESSION_CAPACITY {
            self.entries
                .iter()
                .min_by_key(|(_, existing)| (existing.last_seen, existing.created_at))
                .map(|(digest, _)| *digest)
        } else {
            None
        };
        if let Some(oldest) = oldest {
            self.entries.remove(&oldest);
        }

        self.entries.insert(token_digest, session);
        Ok(())
    }
}

/// Immutable account configuration loaded at startup.
///
/// Password hashes stay private and custom `Debug` output only exposes the
/// configured usernames. Cloning this value never clones or shares runtime
/// sessions; each server constructs its own [`AccessControl`].
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AuthConfig {
    users: HashMap<String, String>,
}

impl AuthConfig {
    pub fn new(raw_accounts: &[&str]) -> Result<Self> {
        if raw_accounts.len() > MAX_ACCOUNTS {
            bail!("At most {MAX_ACCOUNTS} auth accounts may be configured");
        }
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

        Ok(Self { users })
    }

    pub fn has_users(&self) -> bool {
        !self.users.is_empty()
    }
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut users: Vec<_> = self.users.keys().collect();
        users.sort_unstable();
        f.debug_struct("AuthConfig").field("users", &users).finish()
    }
}

impl<'de> Deserialize<'de> for AuthConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let accounts = Vec::<String>::deserialize(deserializer)?;
        let accounts: Vec<_> = accounts.iter().map(String::as_str).collect();
        Self::new(&accounts).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone)]
pub struct AccessControl {
    config: AuthConfig,
    sessions: Arc<Mutex<SessionStore>>,
}

impl Default for AccessControl {
    fn default() -> Self {
        Self::from_config(AuthConfig::default())
    }
}

impl fmt::Debug for AccessControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut users: Vec<_> = self.config.users.keys().collect();
        users.sort_unstable();
        f.debug_struct("AccessControl")
            .field("users", &users)
            .field("active_sessions", &self.lock_sessions().entries.len())
            .finish()
    }
}

impl PartialEq for AccessControl {
    fn eq(&self, other: &Self) -> bool {
        self.config == other.config
    }
}

impl AccessControl {
    /// Create a fresh runtime authentication service from static account
    /// configuration. The new service never shares sessions with another
    /// service created from the same configuration.
    pub fn from_config(config: AuthConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(SessionStore::default())),
        }
    }

    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

    pub fn has_users(&self) -> bool {
        self.config.has_users()
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
                csrf_token: encode_hex(csrf_token),
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
            .config
            .users
            .get(user)
            .map(|hash| (hash, true))
            .or_else(|| self.config.users.values().next().map(|hash| (hash, false)))
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

impl From<AuthConfig> for AccessControl {
    fn from(config: AuthConfig) -> Self {
        Self::from_config(config)
    }
}

/// Hash a password with Argon2id v19 and the crate's standard parameters.
pub fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() {
        bail!("Password must not be empty");
    }
    if password.len() > MAX_PASSWORD_BYTES {
        bail!("Password exceeds the {MAX_PASSWORD_BYTES}-byte limit");
    }
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
    Ok(encode_hex(random_bytes()?))
}

fn session_digest(token: &str) -> [u8; SECRET_BYTES] {
    Sha256::digest(token.as_bytes()).into()
}

fn is_encoded_secret(value: &str) -> bool {
    value.len() == ENCODED_SECRET_LEN && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn decode_secret_for_comparison(value: &str) -> ([u8; SECRET_BYTES], bool) {
    let mut decoded = [0u8; SECRET_BYTES];
    let valid = value.len() == ENCODED_SECRET_LEN && decode_hex_to_slice(value, &mut decoded);
    (decoded, valid)
}

#[cfg(test)]
mod tests;
