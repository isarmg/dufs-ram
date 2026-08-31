use crate::utils::encode_hex;
use anyhow::{Context, Result, anyhow, bail};
use headers::HeaderValue;
use hyper::{HeaderMap, header::COOKIE};
use rustix::time::{ClockId, clock_gettime};
use sarmg_admin_auth::{
    hash_password as foundation_hash_password, is_token_shape,
    parse_cookie_value as parse_foundation_cookie_value, random_token,
    require_canonical_administrator_username, require_csrf_token_matches_hash, token_hash,
    token_matches_hash, verify_password as foundation_verify_password,
};
use sarmg_contracts::AdministratorSession;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

pub const SESSION_COOKIE_NAME: &str = "__Host-dufs-session";
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const SESSION_ABSOLUTE_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);
pub use sarmg_admin_auth::PASSWORD_MAX_BYTES as MAX_PASSWORD_BYTES;

const SESSION_CAPACITY: usize = 1024;
const SESSION_PER_USER_CAPACITY: usize = 32;
const MAX_ACCOUNTS: usize = 1024;
const SECRET_BYTES: usize = 32;
const COOKIE_ATTRIBUTES: &str = "Path=/; HttpOnly; Secure; SameSite=Strict";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Configured administrator username. It also remains the product-local owner
    /// key for durable file operations.
    pub user: String,
    pub csrf_token: String,
}

impl SessionInfo {
    /// Materialize the one current Foundation browser-session contract.
    pub fn administrator_session(&self) -> Result<AdministratorSession> {
        AdministratorSession::new(
            stable_administrator_id(&self.user),
            self.user.clone(),
            self.csrf_token.clone(),
        )
        .context("Configured administrator cannot be represented by the current auth contract")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub token: String,
    pub session: SessionInfo,
}

/// Proof that [`AccessControl`] has verified an account password.
///
/// The administrator username is intentionally private and this type is not
/// cloneable, so a session can only be created from a successful verification
/// result.
pub(crate) struct VerifiedUser {
    user: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SessionInstant(Duration);

impl SessionInstant {
    fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

#[cfg(test)]
impl std::ops::Add<Duration> for SessionInstant {
    type Output = Self;

    fn add(self, duration: Duration) -> Self::Output {
        Self(
            self.0
                .checked_add(duration)
                .expect("test session time is representable"),
        )
    }
}

#[derive(Clone)]
enum SessionClock {
    Boottime,
    #[cfg(test)]
    Injected(Arc<dyn Fn() -> SessionInstant + Send + Sync>),
}

impl SessionClock {
    fn boottime() -> Self {
        Self::Boottime
    }

    #[cfg(test)]
    fn injected(now: impl Fn() -> SessionInstant + Send + Sync + 'static) -> Self {
        Self::Injected(Arc::new(now))
    }

    fn now(&self) -> SessionInstant {
        match self {
            Self::Boottime => SessionInstant(
                Duration::try_from(clock_gettime(ClockId::Boottime))
                    .expect("CLOCK_BOOTTIME is a non-negative duration"),
            ),
            #[cfg(test)]
            Self::Injected(now) => now(),
        }
    }
}

#[derive(Debug)]
struct SessionRecord {
    user: String,
    csrf_token: String,
    created_at: SessionInstant,
    last_seen: SessionInstant,
}

impl SessionRecord {
    fn is_expired(&self, now: SessionInstant) -> bool {
        now.saturating_duration_since(self.created_at) >= SESSION_ABSOLUTE_TIMEOUT
            || now.saturating_duration_since(self.last_seen) >= SESSION_IDLE_TIMEOUT
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            user: self.user.clone(),
            csrf_token: self.csrf_token.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct SessionStore {
    entries: HashMap<[u8; SECRET_BYTES], SessionRecord>,
}

impl SessionStore {
    fn purge_expired(&mut self, now: SessionInstant) {
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
/// configured administrator usernames. Cloning this value never clones or
/// shares runtime sessions; each server constructs its own [`AccessControl`].
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
                anyhow!(
                    "Invalid auth account #{account_number}: expected `username:<argon2id PHC>`"
                )
            })?;
            if user.is_empty() || password.is_empty() {
                bail!(
                    "Invalid auth account #{account_number}: administrator username and Argon2id PHC must not be empty"
                );
            }
            require_canonical_administrator_username(user).with_context(|| {
                format!("Invalid administrator username in auth account #{account_number}")
            })?;
            if users.contains_key(user) {
                bail!("Invalid auth account #{account_number}: duplicate administrator username");
            }

            sarmg_admin_auth::require_current_password_hash(password).with_context(|| {
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
    clock: SessionClock,
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
        Self::with_clock(config, SessionClock::boottime())
    }

    fn with_clock(config: AuthConfig, clock: SessionClock) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(SessionStore::default())),
            clock,
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
        let token = random_token().context("Failed to generate a session token")?;
        let token_digest = token_hash(&token);
        let csrf_token = random_token().context("Failed to generate a CSRF token")?;
        let now = self.clock.now();

        let mut sessions = self.lock_sessions();
        sessions.purge_expired(now);
        if sessions.entries.contains_key(&token_digest) {
            bail!("Generated a duplicate session token");
        }
        // Check the new digest before removing the previous session, then
        // replace under the same lock. This preserves the previous session on a
        // random-token collision and avoids evicting an unrelated session
        // when a full store rotates one of its existing entries.
        if let Some(previous_token) = previous_token
            && is_token_shape(previous_token)
        {
            sessions.entries.remove(&token_hash(previous_token));
        }
        sessions.insert(
            token_digest,
            SessionRecord {
                user: user.clone(),
                csrf_token: csrf_token.clone(),
                created_at: now,
                last_seen: now,
            },
        )?;

        Ok(CreatedSession {
            token,
            session: SessionInfo { user, csrf_token },
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
        self.authenticate_at(token, self.clock.now())
    }

    /// Remove a session. Returns whether an active or expired record existed.
    pub fn logout(&self, token: &str) -> bool {
        if !is_token_shape(token) {
            return false;
        }
        self.lock_sessions()
            .entries
            .remove(&token_hash(token))
            .is_some()
    }

    /// Check the CSRF value belonging to an unexpired session.
    ///
    /// Both supplied values must use the one current Foundation token shape,
    /// and both are compared against the session-bound 256-bit token hash.
    pub fn verify_csrf_header_values<Value>(
        &self,
        token: &str,
        expected: &str,
        candidate_values: &[Value],
    ) -> bool
    where
        Value: AsRef<[u8]>,
    {
        if !is_token_shape(token) {
            return false;
        }

        let now = self.clock.now();
        let digest = token_hash(token);
        let mut sessions = self.lock_sessions();
        let Some(session) = sessions.entries.get(&digest) else {
            return false;
        };
        if session.is_expired(now) {
            sessions.entries.remove(&digest);
            return false;
        }

        let csrf_digest = token_hash(&session.csrf_token);
        is_token_shape(expected)
            && token_matches_hash(expected, &csrf_digest)
            && require_csrf_token_matches_hash(candidate_values, &csrf_digest).is_ok()
    }

    #[cfg(test)]
    fn verify_csrf(&self, token: &str, expected: &str, candidate: &str) -> bool {
        self.verify_csrf_header_values(token, expected, &[candidate.as_bytes()])
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

        let verified = foundation_verify_password(password, password_hash);
        known_user && verified
    }

    fn authenticate_at(&self, token: &str, now: SessionInstant) -> Option<SessionInfo> {
        if !is_token_shape(token) {
            return None;
        }

        let digest = token_hash(token);
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
        session.last_seen = session.last_seen.max(now);
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

/// Hash a password with Foundation's one current administrator policy.
pub fn hash_password(password: &str) -> Result<String> {
    foundation_hash_password(password).context("Failed to hash administrator password")
}

pub fn session_cookie(token: &str) -> Result<HeaderValue> {
    if !is_token_shape(token) {
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
    let raw = cookie.to_str().ok()?;
    let value = parse_foundation_cookie_value(raw, SESSION_COOKIE_NAME)?;
    is_token_shape(value).then_some(value)
}

/// Extract the current session cookie only when the request has exactly one
/// unambiguous raw Cookie header line. Multiple header lines, duplicate
/// session-cookie names, empty values, malformed UTF-8 and non-current token
/// shapes are explicit errors so a login request cannot silently treat an
/// attacker-controlled cookie as absent.
pub(crate) fn session_token_from_headers(
    headers: &HeaderMap,
) -> std::result::Result<Option<&str>, ()> {
    let mut values = headers.get_all(COOKIE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let raw = value.to_str().map_err(|_| ())?;
    if let Some(token) = parse_foundation_cookie_value(raw, SESSION_COOKIE_NAME) {
        return is_token_shape(token).then_some(token).ok_or(()).map(Some);
    }

    let contains_session_cookie = raw.split(';').any(|part| {
        let part = part.trim();
        part == SESSION_COOKIE_NAME
            || part
                .split_once('=')
                .is_some_and(|(name, _)| name == SESSION_COOKIE_NAME)
    });
    if contains_session_cookie {
        Err(())
    } else {
        Ok(None)
    }
}

fn stable_administrator_id(username: &str) -> String {
    format!("dufs:{}", encode_hex(Sha256::digest(username.as_bytes())))
}

#[cfg(test)]
mod tests;
