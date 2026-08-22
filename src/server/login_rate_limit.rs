use super::identity::OwnerId;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const GLOBAL_BURST: f64 = 16.0;
const GLOBAL_REFILL_PER_SECOND: f64 = 1.0;
const ADDRESS_BURST: f64 = 8.0;
const ADDRESS_REFILL_PER_SECOND: f64 = 1.0;
const FAILURE_BACKOFF_THRESHOLD: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const ENTRY_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub(super) struct LoginRateLimiter {
    state: Arc<Mutex<LoginRateState>>,
}

#[derive(Debug)]
struct LoginRateState {
    global_tokens: f64,
    global_last_refill: Instant,
    request_addresses: HashMap<IpAddr, RequestBudget>,
    credential_failures: HashMap<CredentialKey, FailureState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct CredentialKey {
    address: IpAddr,
    account: [u8; 32],
}

#[derive(Clone, Copy, Debug)]
struct RequestBudget {
    tokens: f64,
    last_refill: Instant,
    last_seen: Instant,
}

#[derive(Clone, Copy, Debug)]
struct FailureState {
    failures: u32,
    blocked_until: Instant,
    last_seen: Instant,
}

impl LoginRateLimiter {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        Self {
            state: Arc::new(Mutex::new(LoginRateState {
                global_tokens: GLOBAL_BURST,
                global_last_refill: now,
                request_addresses: HashMap::new(),
                credential_failures: HashMap::new(),
            })),
        }
    }

    /// Charge the cheap global and source-address budgets before reading or
    /// parsing a login request body.
    pub(super) fn check_request(&self, address: IpAddr) -> Result<(), Duration> {
        self.check_request_at(address, Instant::now())
    }

    pub(super) fn check_account_backoff(
        &self,
        address: IpAddr,
        username: &str,
    ) -> Result<(), Duration> {
        self.check_account_at(address, account_key(username), Instant::now())
    }

    pub(super) fn record_failure(&self, address: IpAddr, username: &str) {
        self.record_failure_at(address, account_key(username), Instant::now());
    }

    pub(super) fn record_success(&self, address: IpAddr, username: &str) {
        self.record_success_for(address, account_key(username));
    }

    fn record_success_for(&self, address: IpAddr, account: [u8; 32]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .credential_failures
            .remove(&CredentialKey { address, account });
    }

    fn check_request_at(&self, address: IpAddr, now: Instant) -> Result<(), Duration> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.purge(now);
        state.refill_global(now);
        if state.global_tokens < 1.0 {
            return Err(Duration::from_secs(1));
        }

        let address_budget = state
            .request_addresses
            .entry(address)
            .or_insert(RequestBudget {
                tokens: ADDRESS_BURST,
                last_refill: now,
                last_seen: now,
            });
        address_budget.refill(now);
        if address_budget.tokens < 1.0 {
            return Err(Duration::from_secs(1));
        }

        address_budget.tokens -= 1.0;
        address_budget.last_seen = now;
        state.global_tokens -= 1.0;
        Ok(())
    }

    fn check_account_at(
        &self,
        address: IpAddr,
        account: [u8; 32],
        now: Instant,
    ) -> Result<(), Duration> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.purge(now);
        let delay = state
            .credential_failures
            .get(&CredentialKey { address, account })
            .map(|entry| entry.blocked_until.saturating_duration_since(now))
            .unwrap_or_default();
        if !delay.is_zero() {
            return Err(delay);
        }
        Ok(())
    }

    fn record_failure_at(&self, address: IpAddr, account: [u8; 32], now: Instant) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update_failure(
            &mut state.credential_failures,
            CredentialKey { address, account },
            now,
        );
    }
}

impl LoginRateState {
    fn refill_global(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.global_last_refill);
        self.global_tokens = (self.global_tokens
            + elapsed.as_secs_f64() * GLOBAL_REFILL_PER_SECOND)
            .min(GLOBAL_BURST);
        self.global_last_refill = now;
    }

    fn purge(&mut self, now: Instant) {
        self.request_addresses
            .retain(|_, entry| now.saturating_duration_since(entry.last_seen) < ENTRY_TTL);
        self.credential_failures
            .retain(|_, entry| now.saturating_duration_since(entry.last_seen) < ENTRY_TTL);
    }
}

impl RequestBudget {
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * ADDRESS_REFILL_PER_SECOND).min(ADDRESS_BURST);
        self.last_refill = now;
        self.last_seen = now;
    }
}

fn update_failure<K: Eq + std::hash::Hash>(
    entries: &mut HashMap<K, FailureState>,
    key: K,
    now: Instant,
) {
    let entry = entries.entry(key).or_insert(FailureState {
        failures: 0,
        blocked_until: now,
        last_seen: now,
    });
    entry.failures = entry.failures.saturating_add(1);
    entry.last_seen = now;
    if entry.failures >= FAILURE_BACKOFF_THRESHOLD {
        let exponent = entry.failures.saturating_sub(FAILURE_BACKOFF_THRESHOLD);
        let seconds = 1_u64.checked_shl(exponent.min(6)).unwrap_or(64);
        entry.blocked_until = now + Duration::from_secs(seconds).min(MAX_BACKOFF);
    }
}

fn account_key(username: &str) -> [u8; 32] {
    OwnerId::login_throttle(username).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_failures_back_off_only_the_same_address_and_account_pair() {
        let limiter = LoginRateLimiter::new();
        let address: IpAddr = "192.0.2.10".parse().unwrap();
        let other_address: IpAddr = "192.0.2.11".parse().unwrap();
        let now = Instant::now();
        let account = account_key("alice");

        for offset in 0..FAILURE_BACKOFF_THRESHOLD {
            limiter.record_failure_at(address, account, now + Duration::from_millis(offset.into()));
        }
        assert!(
            limiter
                .check_account_at(address, account, now + Duration::from_millis(10))
                .is_err(),
            "the failing address/account pair must be backed off"
        );
        assert!(
            limiter
                .check_account_at(
                    address,
                    account_key("other"),
                    now + Duration::from_millis(10)
                )
                .is_ok(),
            "another account from the same address must not share failure backoff"
        );
        assert!(
            limiter
                .check_account_at(other_address, account, now + Duration::from_millis(10))
                .is_ok(),
            "the same account from another address must not share failure backoff"
        );
    }

    #[test]
    fn success_clears_only_the_matching_address_and_account_pair() {
        let limiter = LoginRateLimiter::new();
        let first_address: IpAddr = "192.0.2.20".parse().unwrap();
        let second_address: IpAddr = "192.0.2.21".parse().unwrap();
        let account = account_key("alice");
        let now = Instant::now();

        for offset in 0..FAILURE_BACKOFF_THRESHOLD {
            let failure_time = now + Duration::from_millis(offset.into());
            limiter.record_failure_at(first_address, account, failure_time);
            limiter.record_failure_at(second_address, account, failure_time);
        }
        limiter.record_success_for(first_address, account);

        assert!(
            limiter
                .check_account_at(first_address, account, now + Duration::from_millis(10))
                .is_ok(),
            "success must clear the matching pair's failure history"
        );
        assert!(
            limiter
                .check_account_at(second_address, account, now + Duration::from_millis(10))
                .is_err(),
            "success must not clear another address's failure history"
        );
    }

    #[test]
    fn global_bucket_bounds_rotating_identifiers() {
        let limiter = LoginRateLimiter::new();
        let now = Instant::now();
        for index in 0..GLOBAL_BURST as u8 {
            limiter
                .check_request_at(IpAddr::from([198, 51, 100, index]), now)
                .unwrap();
        }
        assert!(
            limiter
                .check_request_at("203.0.113.1".parse().unwrap(), now)
                .is_err()
        );
        assert!(
            limiter
                .check_request_at("203.0.113.1".parse().unwrap(), now + Duration::from_secs(1))
                .is_ok()
        );
    }

    #[test]
    fn source_budget_bounds_malformed_or_rotating_account_attempts() {
        let limiter = LoginRateLimiter::new();
        let address = "192.0.2.50".parse().unwrap();
        let now = Instant::now();
        for _ in 0..ADDRESS_BURST as usize {
            limiter.check_request_at(address, now).unwrap();
        }
        assert!(limiter.check_request_at(address, now).is_err());
        assert!(
            limiter
                .check_request_at(address, now + Duration::from_secs(1))
                .is_ok()
        );
    }

    #[test]
    fn account_backoff_does_not_charge_the_pre_parse_budget_twice() {
        let limiter = LoginRateLimiter::new();
        let address = "192.0.2.51".parse().unwrap();
        let now = Instant::now();
        limiter.check_request_at(address, now).unwrap();
        limiter
            .check_account_at(address, account_key("alice"), now)
            .unwrap();
        assert_eq!(
            limiter
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .global_tokens,
            GLOBAL_BURST - 1.0
        );
    }
}
