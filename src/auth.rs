//! Product configuration syntax only; all runtime authentication belongs to Foundation.
use anyhow::{Context, Result, anyhow, bail};
use sarmg_admin_auth::require_canonical_administrator_username;
use sarmg_admin_core::{AdministratorRecord, AdministratorService, Identifier};
use sarmg_admin_static::StaticAdministratorStore;
use serde::{Deserialize, Deserializer};
use std::{collections::HashMap, fmt, sync::Arc};

/// File operations retain the configured username as their business owner key.
#[derive(Clone, Debug)]
pub struct FilePrincipal {
    pub username: String,
}

/// Immutable account configuration loaded at startup.
///
/// Password hashes stay private and custom `Debug` output only exposes the
/// configured administrator usernames. Cloning this value never clones or
/// shares runtime sessions; each server constructs its own Foundation service.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AuthConfig {
    users: HashMap<String, String>,
}

impl AuthConfig {
    pub fn new(raw_accounts: &[&str]) -> Result<Self> {
        if raw_accounts.len() > sarmg_admin_core::STATIC_ADMINISTRATORS_MAX {
            bail!("Static account capacity exceeds the Foundation profile");
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

impl AuthConfig {
    pub(crate) fn administrator_service(
        &self,
    ) -> Result<Arc<AdministratorService<StaticAdministratorStore>>> {
        let records = self
            .users
            .iter()
            .map(|(username, hash)| {
                Ok(AdministratorRecord {
                    administrator_id: Identifier::new(username.clone())?,
                    username: username.clone(),
                    password_hash: hash.clone(),
                    active: true,
                    session_version: 1,
                    created_at_micros: 0,
                    updated_at_micros: 0,
                    last_login_at_micros: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(AdministratorService::new(
            StaticAdministratorStore::new(records)?,
        )))
    }
}

pub fn hash_password(password: &str) -> Result<String> {
    sarmg_admin_auth::hash_password(password).context("Failed to hash administrator password")
}

#[cfg(test)]
mod tests;
