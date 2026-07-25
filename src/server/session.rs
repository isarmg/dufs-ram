use super::{Request, Response, Server, status_error};
use crate::{
    auth::{MAX_USERNAME_BYTES, session_token_from_cookie},
    http_utils::body_full,
};

use anyhow::{Result, anyhow};
use headers::{ContentLength, ContentType, HeaderMapExt};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::{
    Method, StatusCode, Uri,
    header::{
        ACCEPT, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, HeaderMap, HeaderValue,
        LOCATION, REFERRER_POLICY, SET_COOKIE,
    },
};
use std::{
    collections::{HashMap, hash_map::Entry},
    time::{Duration, Instant},
};
use tokio::sync::OwnedSemaphorePermit;

pub(super) const LOGIN_PATH: &str = "__dufs__/login";
pub(super) const LOGOUT_PATH: &str = "__dufs__/logout";
pub(super) const CSRF_HEADER: &str = "x-dufs-csrf-token";
pub(super) const LOGIN_ERROR_QUERY: &str = "login_error";

const LOGIN_HTML: &str = include_str!("../../assets/login.html");
const LOGIN_BODY_LIMIT: usize = 4 * 1024;
const LOGIN_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";
const LOGIN_ERROR_CAPACITY: usize = 1024;
const LOGIN_ERROR_TOKEN_BYTES: usize = 32;
const LOGIN_ERROR_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginError {
    MissingFields,
    InvalidCredentials,
    TooManyRequests,
}

impl LoginError {
    const fn message(self) -> &'static str {
        match self {
            Self::MissingFields => "请填写账号和密码",
            Self::InvalidCredentials => "用户名或密码错误。",
            Self::TooManyRequests => "登录请求过多，请稍后重试。",
        }
    }
}

#[derive(Debug)]
struct LoginErrorRecord {
    error: LoginError,
    created_at: Instant,
}

#[derive(Debug, Default)]
pub(super) struct LoginErrorStore {
    entries: HashMap<[u8; LOGIN_ERROR_TOKEN_BYTES], LoginErrorRecord>,
}

impl LoginErrorStore {
    fn insert(&mut self, error: LoginError) -> Result<String> {
        let now = Instant::now();
        self.purge_expired(now);
        if self.entries.len() >= LOGIN_ERROR_CAPACITY
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, record)| record.created_at)
                .map(|(token, _)| *token)
        {
            self.entries.remove(&oldest);
        }

        for _ in 0..4 {
            let mut token = [0u8; LOGIN_ERROR_TOKEN_BYTES];
            getrandom::fill(&mut token)
                .map_err(|err| anyhow!("Failed to generate login error token: {err}"))?;
            if let Entry::Vacant(entry) = self.entries.entry(token) {
                entry.insert(LoginErrorRecord {
                    error,
                    created_at: now,
                });
                return Ok(hex::encode(token));
            }
        }
        Err(anyhow!("Failed to generate a unique login error token"))
    }

    fn consume(&mut self, encoded_token: &str) -> Option<LoginError> {
        let token = decode_login_error_token(encoded_token)?;
        self.purge_expired(Instant::now());
        self.entries.remove(&token).map(|record| record.error)
    }

    fn purge_expired(&mut self, now: Instant) {
        self.entries
            .retain(|_, record| now.saturating_duration_since(record.created_at) < LOGIN_ERROR_TTL);
    }
}

impl Server {
    pub(super) async fn handle_login(
        &self,
        req: Request,
        res: &mut Response,
    ) -> Result<Option<String>> {
        if !request_source_is_same_origin(req.headers(), req.uri()) {
            status_error(res, StatusCode::FORBIDDEN, "Forbidden");
            self.add_private_security_headers(res);
            return Ok(None);
        }

        let content_type_ok = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| {
                value
                    .trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            });
        if !content_type_ok {
            status_error(
                res,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/x-www-form-urlencoded",
            );
            self.add_private_security_headers(res);
            return Ok(None);
        }

        let previous_token = req
            .headers()
            .get(hyper::header::COOKIE)
            .and_then(session_token_from_cookie)
            .map(str::to_owned);
        let body = match Limited::new(req.into_body(), LOGIN_BODY_LIMIT)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(err) => {
                if err.downcast_ref::<LengthLimitError>().is_some() {
                    status_error(res, StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
                } else {
                    status_error(res, StatusCode::BAD_REQUEST, "Invalid request body");
                }
                self.add_private_security_headers(res);
                return Ok(None);
            }
        };

        let Some((username, password)) = parse_login_form(&body) else {
            self.redirect_login_error(res, LoginError::InvalidCredentials)?;
            return Ok(None);
        };
        if username.is_empty() || password.is_empty() {
            self.redirect_login_error(res, LoginError::MissingFields)?;
            return Ok(None);
        }

        let Ok(permit) = self.login_slots.clone().try_acquire_owned() else {
            self.redirect_login_error(res, LoginError::TooManyRequests)?;
            return Ok(None);
        };

        let auth = self.args.auth.clone();
        let login_user = username.clone();
        let verified_user = run_with_login_slot(permit, move || {
            auth.verify_credentials(&login_user, &password)
        })
        .await?;

        let Some(verified_user) = verified_user else {
            self.redirect_login_error(res, LoginError::InvalidCredentials)?;
            return Ok(None);
        };
        let created = self
            .args
            .auth
            .create_session(verified_user, previous_token.as_deref())?;

        res.headers_mut()
            .insert(SET_COOKIE, self.args.auth.session_cookie(&created.token)?);
        res.headers_mut().insert(
            LOCATION,
            HeaderValue::from_str(self.args.uri_prefix.as_str())?,
        );
        *res.status_mut() = StatusCode::SEE_OTHER;
        self.add_private_security_headers(res);
        Ok(Some(created.session.user))
    }

    pub(super) fn send_login_page_for_get(
        &self,
        login_error_token: Option<&str>,
        res: &mut Response,
    ) -> Result<()> {
        let error = login_error_token.and_then(|token| {
            self.login_errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .consume(token)
        });
        self.send_login_page(res, StatusCode::OK, error.map(LoginError::message))?;
        Ok(())
    }

    fn send_login_page(
        &self,
        res: &mut Response,
        status: StatusCode,
        error: Option<&str>,
    ) -> Result<()> {
        let login_action = format!("{}{}", self.args.uri_prefix, LOGIN_PATH);
        let output = LOGIN_HTML
            .replace("__LOGIN_ACTION__", &login_action)
            .replace(
                "__ERROR_CLASS__",
                if error.is_some() { "" } else { "hidden" },
            )
            .replace("__ERROR_MESSAGE__", error.unwrap_or_default());
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.status_mut() = status;
        *res.body_mut() = body_full(output);
        self.add_private_security_headers(res);
        Ok(())
    }

    fn redirect_login_error(&self, res: &mut Response, error: LoginError) -> Result<()> {
        let token = self
            .login_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(error)?;
        let location = format!(
            "{}{}?{}={token}",
            self.args.uri_prefix, LOGIN_PATH, LOGIN_ERROR_QUERY
        );
        res.headers_mut()
            .insert(LOCATION, HeaderValue::from_str(&location)?);
        *res.status_mut() = StatusCode::SEE_OTHER;
        self.add_private_security_headers(res);
        Ok(())
    }

    pub(super) fn reject_unauthenticated(
        &self,
        method: &Method,
        headers: &HeaderMap,
        res: &mut Response,
    ) -> Result<()> {
        res.headers_mut()
            .insert(SET_COOKIE, self.args.auth.clear_session_cookie());
        if matches!(*method, Method::GET | Method::HEAD) && accepts_html(headers) {
            let location = format!("{}{}", self.args.uri_prefix, LOGIN_PATH);
            res.headers_mut()
                .insert(LOCATION, HeaderValue::from_str(&location)?);
            *res.status_mut() = StatusCode::SEE_OTHER;
        } else {
            status_error(res, StatusCode::UNAUTHORIZED, "Authentication required");
        }
        self.add_private_security_headers(res);
        Ok(())
    }

    pub(super) fn handle_logout(&self, session_token: &str, res: &mut Response) {
        self.args.auth.logout(session_token);
        res.headers_mut()
            .insert(SET_COOKIE, self.args.auth.clear_session_cookie());
        *res.status_mut() = StatusCode::NO_CONTENT;
        self.add_private_security_headers(res);
    }

    pub(super) fn csrf_is_valid(
        &self,
        headers: &HeaderMap,
        request_uri: &Uri,
        session_token: &str,
        expected_csrf: &str,
    ) -> bool {
        if !request_source_is_same_origin(headers, request_uri) {
            return false;
        }
        headers
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|candidate| {
                self.args
                    .auth
                    .verify_csrf(session_token, expected_csrf, candidate)
            })
    }

    pub(super) fn add_private_security_headers(&self, res: &mut Response) {
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
        res.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        res.headers_mut()
            .insert("x-frame-options", HeaderValue::from_static("DENY"));
        res.headers_mut()
            .insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        res.headers_mut().insert(
            "permissions-policy",
            HeaderValue::from_static(
                "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
            ),
        );
        res.headers_mut()
            .insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(LOGIN_CSP));
    }
}

async fn run_with_login_slot<T, F>(permit: OwnedSemaphorePermit, task: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        task()
    })
    .await?)
}

fn decode_login_error_token(encoded_token: &str) -> Option<[u8; LOGIN_ERROR_TOKEN_BYTES]> {
    if encoded_token.len() != LOGIN_ERROR_TOKEN_BYTES * 2 {
        return None;
    }
    let mut token = [0u8; LOGIN_ERROR_TOKEN_BYTES];
    hex::decode_to_slice(encoded_token, &mut token).ok()?;
    Some(token)
}

fn parse_login_form(body: &[u8]) -> Option<(String, String)> {
    let mut values: HashMap<String, String> = HashMap::new();
    for (key, value) in form_urlencoded::parse(body) {
        if !matches!(key.as_ref(), "username" | "password")
            || values
                .insert(key.into_owned(), value.into_owned())
                .is_some()
        {
            return None;
        }
    }
    let username = values.remove("username")?;
    let password = values.remove("password")?;
    if username.len() > MAX_USERNAME_BYTES || password.len() > 1024 || !values.is_empty() {
        return None;
    }
    Some((username, password))
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|item| item.trim().starts_with("text/html"))
        })
}

fn request_source_is_same_origin(headers: &HeaderMap, request_uri: &Uri) -> bool {
    let fetch_site = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok());
    if fetch_site.is_some_and(|value| value.eq_ignore_ascii_case("cross-site")) {
        return false;
    }

    let Some(origin) = headers.get("origin").and_then(|value| value.to_str().ok()) else {
        return true;
    };
    let request_authority = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| request_uri.authority().map(|authority| authority.as_str()));
    let Some(request_authority) = request_authority else {
        return false;
    };
    if origin.eq_ignore_ascii_case("null") {
        return fetch_site.is_some_and(|value| value.eq_ignore_ascii_case("same-origin"));
    }
    let Ok(origin) = origin.parse::<Uri>() else {
        return false;
    };
    origin
        .authority()
        .is_some_and(|authority| authority.as_str().eq_ignore_ascii_case(request_authority))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::{Semaphore, oneshot};

    #[test]
    fn login_form_requires_exact_fields() {
        assert_eq!(
            parse_login_form(b"username=user&password=pa%3Ass"),
            Some(("user".to_string(), "pa:ss".to_string()))
        );
        assert!(parse_login_form(b"username=user").is_none());
        assert_eq!(
            parse_login_form(b"username=&password=pass"),
            Some(("".to_string(), "pass".to_string()))
        );
        assert_eq!(
            parse_login_form(b"username=user&password="),
            Some(("user".to_string(), "".to_string()))
        );
        assert!(parse_login_form(b"username=a&username=b&password=p").is_none());
        assert!(parse_login_form(b"username=user&password=pass&extra=1").is_none());
        let maximum_username = "u".repeat(MAX_USERNAME_BYTES);
        let maximum_form = format!("username={maximum_username}&password=p");
        assert_eq!(
            parse_login_form(maximum_form.as_bytes()),
            Some((maximum_username, "p".to_string()))
        );
        let oversized_form = format!("username={}&password=p", "u".repeat(MAX_USERNAME_BYTES + 1));
        assert!(parse_login_form(oversized_form.as_bytes()).is_none());
    }

    #[test]
    fn login_error_tokens_are_random_and_consumed_once() -> Result<()> {
        let mut store = LoginErrorStore::default();
        let missing = store.insert(LoginError::MissingFields)?;
        let invalid = store.insert(LoginError::InvalidCredentials)?;
        assert_eq!(missing.len(), LOGIN_ERROR_TOKEN_BYTES * 2);
        assert_ne!(missing, invalid);
        assert_eq!(store.consume(&missing), Some(LoginError::MissingFields));
        assert_eq!(store.consume(&missing), None);
        assert_eq!(
            store.consume(&invalid),
            Some(LoginError::InvalidCredentials)
        );
        assert_eq!(store.consume("untrusted-text"), None);

        let expired_token = [0xA5; LOGIN_ERROR_TOKEN_BYTES];
        store.entries.insert(
            expired_token,
            LoginErrorRecord {
                error: LoginError::TooManyRequests,
                created_at: Instant::now()
                    .checked_sub(LOGIN_ERROR_TTL)
                    .ok_or_else(|| anyhow!("Could not construct expired test instant"))?,
            },
        );
        assert_eq!(store.consume(&hex::encode(expired_token)), None);

        for _ in 0..=LOGIN_ERROR_CAPACITY {
            store.insert(LoginError::MissingFields)?;
        }
        assert_eq!(store.entries.len(), LOGIN_ERROR_CAPACITY);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_login_waiter_keeps_slot_until_blocking_task_finishes() -> Result<()> {
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned()?;
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();

        let waiter = tokio::spawn(run_with_login_slot(permit, move || {
            let _ = started_tx.send(());
            let _ = release_rx.blocking_recv();
        }));
        started_rx.await?;

        waiter.abort();
        let cancelled = waiter
            .await
            .expect_err("aborted login waiter unexpectedly completed");
        assert!(cancelled.is_cancelled());
        assert!(
            slots.clone().try_acquire_owned().is_err(),
            "cancelling the async waiter released a still-running login slot"
        );

        assert!(release_tx.send(()).is_ok());
        let reacquired =
            tokio::time::timeout(Duration::from_secs(1), slots.clone().acquire_owned()).await??;
        drop(reacquired);
        Ok(())
    }

    #[test]
    fn cross_site_request_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!request_source_is_same_origin(
            &headers,
            &Uri::from_static("/")
        ));
    }

    #[test]
    fn origin_must_match_host_when_present() {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("files.example.test"));
        headers.insert(
            "origin",
            HeaderValue::from_static("https://files.example.test"),
        );
        assert!(request_source_is_same_origin(
            &headers,
            &Uri::from_static("/")
        ));
        headers.insert(
            "origin",
            HeaderValue::from_static("https://evil.example.test"),
        );
        assert!(!request_source_is_same_origin(
            &headers,
            &Uri::from_static("/")
        ));
    }

    #[test]
    fn opaque_origin_requires_browser_same_origin_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", HeaderValue::from_static("null"));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        let uri = Uri::from_static("https://localhost:5000/__dufs__/login");
        assert!(request_source_is_same_origin(&headers, &uri));

        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!request_source_is_same_origin(&headers, &uri));
    }
}
