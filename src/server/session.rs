use super::{
    Request, Response, Server,
    problem::{ApiError, ErrorCode, render_problem},
    status_error,
};
use crate::{
    args::TrustedProxy,
    auth::{MAX_PASSWORD_BYTES, SessionInfo, session_token_from_headers},
    http_utils::{body_full, request_content_type_is},
};

use anyhow::Result;
use headers::{ContentLength, ContentType, HeaderMapExt};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::{
    Method, StatusCode, Uri,
    header::{
        ACCEPT, CACHE_CONTROL, CONTENT_SECURITY_POLICY, HeaderMap, HeaderValue, LOCATION,
        REFERRER_POLICY, RETRY_AFTER, SET_COOKIE,
    },
};
pub(super) use sarmg_admin_auth::CSRF_HEADER;
use sarmg_admin_auth::{
    AdministratorOriginMode, HOST_HEADER, ORIGIN_HEADER, PASSWORD_MIN_BYTES, SEC_FETCH_SITE_HEADER,
    normalize_administrator_username, require_administrator_same_origin,
    require_single_security_header_value, validate_password,
};
pub(super) use sarmg_contracts::{ADMIN_LOGIN_PATH, ADMIN_LOGOUT_PATH, ADMIN_SESSION_PATH};
use sarmg_contracts::{
    AdministratorLoginRequest, AdministratorSession, ErrorCode as FoundationErrorCode,
    ErrorEnvelope,
};
use std::{
    collections::{HashMap, hash_map::Entry},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::OwnedSemaphorePermit;

pub(super) const LOGIN_PATH: &str = "__dufs__/login";
const LOGIN_HTML: &str = include_str!("../../clients/web/login.html");
const LOGIN_JS: &str = include_str!("../../clients/web/login.js");
const LOGIN_URI: &str = "/__dufs__/login";
const LOGIN_BODY_LIMIT: usize = 16 * 1024;
const LOGIN_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_BODY_GLOBAL_LIMIT: usize = 32;
const LOGIN_BODY_PER_IP_LIMIT: usize = 4;
const LOGIN_CSP: &str = "default-src 'none'; script-src 'sha256-8JkQKyZlvHgF9rVyTqmp4acwrwzHlcBJfZeOlicr02c='; style-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

#[derive(Debug, Default)]
struct LoginBodyAdmissionState {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

#[derive(Clone, Debug)]
pub(super) struct LoginBodyAdmission {
    state: Arc<Mutex<LoginBodyAdmissionState>>,
    global_limit: usize,
    per_ip_limit: usize,
}

impl Default for LoginBodyAdmission {
    fn default() -> Self {
        Self::new(LOGIN_BODY_GLOBAL_LIMIT, LOGIN_BODY_PER_IP_LIMIT)
    }
}

impl LoginBodyAdmission {
    fn new(global_limit: usize, per_ip_limit: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(LoginBodyAdmissionState::default())),
            global_limit,
            per_ip_limit,
        }
    }

    fn try_acquire(&self, client_ip: IpAddr) -> Option<LoginBodyPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let per_ip = state.per_ip.get(&client_ip).copied().unwrap_or_default();
        if state.total >= self.global_limit || per_ip >= self.per_ip_limit {
            return None;
        }
        state.total += 1;
        *state.per_ip.entry(client_ip).or_default() += 1;
        Some(LoginBodyPermit {
            admission: self.clone(),
            client_ip,
        })
    }
}

#[derive(Debug)]
struct LoginBodyPermit {
    admission: LoginBodyAdmission,
    client_ip: IpAddr,
}

impl Drop for LoginBodyPermit {
    fn drop(&mut self) {
        let mut state = self
            .admission
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.total = state.total.saturating_sub(1);
        if let Entry::Occupied(mut entry) = state.per_ip.entry(self.client_ip) {
            let remaining = entry.get().saturating_sub(1);
            if remaining == 0 {
                entry.remove();
            } else {
                *entry.get_mut() = remaining;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginError {
    InvalidCredentials,
    TooManyRequests { retry_after_seconds: u64 },
}

impl LoginError {
    const fn message(self) -> &'static str {
        match self {
            Self::InvalidCredentials => "Invalid username or password.",
            Self::TooManyRequests { .. } => "Too many sign-in requests. Please try again later.",
        }
    }

    const fn status(self) -> StatusCode {
        match self {
            Self::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::InvalidCredentials => StatusCode::UNAUTHORIZED,
        }
    }
}

impl Server {
    pub(super) async fn handle_login(
        &self,
        req: Request,
        peer_ip: IpAddr,
        res: &mut Response,
    ) -> Result<Option<String>> {
        if !request_source_is_same_origin(
            req.headers(),
            req.uri(),
            peer_ip,
            &self.content.args.trusted_proxies,
        ) {
            self.render_login_problem(
                res,
                StatusCode::FORBIDDEN,
                "forbidden",
                "Login requests must be same-origin",
                false,
                None,
            )?;
            return Ok(None);
        }

        let client_ip = login_client_ip(req.headers(), peer_ip, &self.content.args.trusted_proxies);
        if let Err(delay) = self.admission.login_rate_limiter.check_request(client_ip) {
            self.render_login_error(
                res,
                LoginError::TooManyRequests {
                    retry_after_seconds: retry_after_seconds(delay),
                },
            )?;
            return Ok(None);
        }

        let content_type_ok = request_content_type_is(req.headers(), "application/json");
        if !content_type_ok {
            self.render_login_problem(
                res,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "Content-Type must be application/json",
                false,
                None,
            )?;
            return Ok(None);
        }

        let Some(body_permit) = self.admission.login_body_admission.try_acquire(client_ip) else {
            self.render_login_error(
                res,
                LoginError::TooManyRequests {
                    retry_after_seconds: 1,
                },
            )?;
            return Ok(None);
        };
        let previous_token = match session_token_from_headers(req.headers()) {
            Ok(token) => token.map(str::to_owned),
            Err(()) => {
                self.render_login_problem(
                    res,
                    StatusCode::BAD_REQUEST,
                    "invalid_cookie_header",
                    "Session Cookie header is invalid or ambiguous",
                    false,
                    None,
                )?;
                return Ok(None);
            }
        };
        let body = match tokio::time::timeout(
            LOGIN_BODY_TIMEOUT,
            Limited::new(req.into_body(), LOGIN_BODY_LIMIT).collect(),
        )
        .await
        {
            Ok(Ok(body)) => body.to_bytes(),
            Ok(Err(err)) => {
                if err.downcast_ref::<LengthLimitError>().is_some() {
                    self.render_login_problem(
                        res,
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_body_too_large",
                        "Login request body is too large",
                        false,
                        None,
                    )?;
                } else {
                    self.render_login_problem(
                        res,
                        StatusCode::BAD_REQUEST,
                        "invalid_request_body",
                        "Login request body is invalid",
                        false,
                        None,
                    )?;
                }
                return Ok(None);
            }
            Err(_) => {
                self.render_login_problem(
                    res,
                    StatusCode::REQUEST_TIMEOUT,
                    "request_timeout",
                    "Login request body timed out",
                    true,
                    None,
                )?;
                return Ok(None);
            }
        };
        drop(body_permit);

        let Ok(credentials) = serde_json::from_slice::<AdministratorLoginRequest>(&body) else {
            self.render_login_problem(
                res,
                StatusCode::BAD_REQUEST,
                "invalid_request_body",
                "Login request must contain exactly non-empty username and password fields",
                false,
                None,
            )?;
            return Ok(None);
        };
        if validate_password(&credentials.password).is_err() {
            self.render_login_problem(
                res,
                StatusCode::BAD_REQUEST,
                "invalid_request_body",
                "Password does not satisfy the current administrator policy",
                false,
                None,
            )?;
            return Ok(None);
        }
        let AdministratorLoginRequest {
            username: untrusted_username,
            password,
        } = credentials;
        let username = match normalize_administrator_username(&untrusted_username) {
            Ok(username) => username,
            Err(_) => {
                self.render_login_problem(
                    res,
                    StatusCode::BAD_REQUEST,
                    "invalid_request_body",
                    "Username is not a valid administrator identity",
                    false,
                    None,
                )?;
                return Ok(None);
            }
        };

        if let Err(delay) = self
            .admission
            .login_rate_limiter
            .check_account_backoff(client_ip, &username)
        {
            self.render_login_error(
                res,
                LoginError::TooManyRequests {
                    retry_after_seconds: retry_after_seconds(delay),
                },
            )?;
            return Ok(None);
        }

        let Ok(permit) = self.admission.login_slots.clone().try_acquire_owned() else {
            self.render_login_error(
                res,
                LoginError::TooManyRequests {
                    retry_after_seconds: 1,
                },
            )?;
            return Ok(None);
        };

        let auth = self.content.auth.clone();
        let login_username = username.clone();
        let verified_user = run_with_login_slot(permit, move || {
            auth.verify_credentials(&login_username, &password)
        })
        .await?;

        let Some(verified_user) = verified_user else {
            self.admission
                .login_rate_limiter
                .record_failure(client_ip, &username);
            self.render_login_error(res, LoginError::InvalidCredentials)?;
            return Ok(None);
        };
        self.admission
            .login_rate_limiter
            .record_success(client_ip, &username);
        let created = self
            .content
            .auth
            .create_session(verified_user, previous_token.as_deref())?;

        res.headers_mut().insert(
            SET_COOKIE,
            self.content.auth.session_cookie(&created.token)?,
        );
        self.send_administrator_session(&created.session, res)?;
        Ok(Some(created.session.user))
    }

    pub(super) fn send_login_page_for_get(&self, res: &mut Response) -> Result<()> {
        let max_password_bytes = MAX_PASSWORD_BYTES.to_string();
        let min_password_bytes = PASSWORD_MIN_BYTES.to_string();
        let output = LOGIN_HTML
            .replace("__ASSETS_PREFIX__", &self.content.assets_prefix)
            .replace("__LOGIN_SCRIPT__", LOGIN_JS)
            .replace("__MIN_PASSWORD_BYTES__", &min_password_bytes)
            .replace("__MAX_PASSWORD_BYTES__", &max_password_bytes);
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.status_mut() = StatusCode::OK;
        *res.body_mut() = body_full(output);
        self.add_private_security_headers(res);
        Ok(())
    }

    pub(super) fn send_administrator_session(
        &self,
        session: &SessionInfo,
        res: &mut Response,
    ) -> Result<()> {
        let contract: AdministratorSession = session.administrator_session()?;
        let output = serde_json::to_vec(&contract)?;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.status_mut() = StatusCode::OK;
        *res.body_mut() = body_full(output);
        self.add_private_security_headers(res);
        Ok(())
    }

    fn render_login_error(&self, res: &mut Response, error: LoginError) -> Result<()> {
        let (code, retryable, retry_after) = match error {
            LoginError::InvalidCredentials => ("invalid_credentials", false, None),
            LoginError::TooManyRequests {
                retry_after_seconds,
            } => ("login_rate_limited", true, Some(retry_after_seconds)),
        };
        self.render_login_problem(
            res,
            error.status(),
            code,
            error.message(),
            retryable,
            retry_after,
        )
    }

    fn render_login_problem(
        &self,
        res: &mut Response,
        status: StatusCode,
        code: &'static str,
        detail: &'static str,
        retryable: bool,
        retry_after: Option<u64>,
    ) -> Result<()> {
        self.render_administrator_auth_error(res, status, code, detail, retryable, retry_after)?;
        Ok(())
    }

    pub(super) fn render_administrator_auth_error(
        &self,
        res: &mut Response,
        status: StatusCode,
        code: &'static str,
        message: &str,
        retryable: bool,
        retry_after: Option<u64>,
    ) -> Result<()> {
        let code = FoundationErrorCode::new(code)
            .expect("administrator auth error codes are compile-time constants");
        let mut envelope = ErrorEnvelope::with_code(code, message).retryable(retryable);
        if let Some(seconds) = retry_after {
            envelope = envelope.with_detail("retry_after", seconds);
            res.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_str(&seconds.to_string())?);
        } else {
            res.headers_mut().remove(RETRY_AFTER);
        }
        let output = serde_json::to_vec(&envelope)?;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.status_mut() = status;
        *res.body_mut() = body_full(output);
        self.add_private_security_headers(res);
        Ok(())
    }

    pub(super) fn reject_unauthenticated(
        &self,
        method: &Method,
        headers: &HeaderMap,
        api_request: bool,
        administrator_auth_api: bool,
        res: &mut Response,
    ) -> Result<()> {
        res.headers_mut()
            .insert(SET_COOKIE, self.content.auth.clear_session_cookie());
        if administrator_auth_api {
            self.render_administrator_auth_error(
                res,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Administrator authentication is required",
                false,
                None,
            )?;
        } else if !api_request
            && matches!(*method, Method::GET | Method::HEAD)
            && accepts_html(headers)
        {
            res.headers_mut()
                .insert(LOCATION, HeaderValue::from_static(LOGIN_URI));
            *res.status_mut() = StatusCode::SEE_OTHER;
        } else if api_request {
            render_problem(
                res,
                &ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    ErrorCode::AUTHENTICATION_REQUIRED,
                    "Authentication required",
                ),
            )?;
        } else {
            status_error(res, StatusCode::UNAUTHORIZED, "Authentication required");
        }
        if !administrator_auth_api {
            self.add_private_security_headers(res);
        }
        Ok(())
    }

    pub(super) fn handle_logout(&self, session_token: &str, res: &mut Response) {
        self.content.auth.logout(session_token);
        res.headers_mut()
            .insert(SET_COOKIE, self.content.auth.clear_session_cookie());
        *res.status_mut() = StatusCode::NO_CONTENT;
        self.add_private_security_headers(res);
    }

    pub(super) fn csrf_is_valid(
        &self,
        headers: &HeaderMap,
        request_uri: &Uri,
        peer_ip: IpAddr,
        session_token: &str,
        expected_csrf: &str,
    ) -> bool {
        if !request_source_is_same_origin(
            headers,
            request_uri,
            peer_ip,
            &self.content.args.trusted_proxies,
        ) {
            return false;
        }
        let candidates = raw_header_values(headers, CSRF_HEADER);
        self.content
            .auth
            .verify_csrf_header_values(session_token, expected_csrf, &candidates)
    }

    pub(super) fn administrator_auth_headers_are_unambiguous(
        &self,
        headers: &HeaderMap,
        request_uri: &Uri,
    ) -> bool {
        let host_values = effective_host_values(headers, request_uri);
        if require_single_security_header_value(HOST_HEADER, &host_values).is_err() {
            return false;
        }
        [ORIGIN_HEADER, SEC_FETCH_SITE_HEADER, CSRF_HEADER]
            .into_iter()
            .all(|name| {
                let values = raw_header_values(headers, name);
                values.is_empty() || require_single_security_header_value(name, &values).is_ok()
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

fn login_client_ip(
    headers: &HeaderMap,
    peer_ip: IpAddr,
    trusted_proxies: &[TrustedProxy],
) -> IpAddr {
    trusted_forwarded_value(headers, "x-forwarded-for", peer_ip, trusted_proxies)
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok())
        .map(canonical_ip)
        .unwrap_or_else(|| canonical_ip(peer_ip))
}

fn trusted_forwarded_value<'a>(
    headers: &'a HeaderMap,
    name: &'static str,
    peer_ip: IpAddr,
    trusted_proxies: &[TrustedProxy],
) -> std::result::Result<Option<&'a str>, ()> {
    if !trusted_proxy_matches(trusted_proxies, peer_ip) {
        return Ok(None);
    }
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(value), None) => {
            let value = value.to_str().map_err(|_| ())?.trim();
            if value.is_empty() || value.contains(',') {
                Err(())
            } else {
                Ok(Some(value))
            }
        }
        (Some(_), Some(_)) => Err(()),
    }
}

fn trusted_proxy_matches(trusted_proxies: &[TrustedProxy], peer_ip: IpAddr) -> bool {
    trusted_proxies.iter().any(|network| {
        network.contains(&peer_ip)
            || match peer_ip {
                IpAddr::V6(address) => address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| network.contains(&IpAddr::V4(mapped))),
                IpAddr::V4(_) => false,
            }
    })
}

fn canonical_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(address)),
        IpAddr::V4(_) => address,
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

fn retry_after_seconds(delay: Duration) -> u64 {
    delay
        .as_secs()
        .saturating_add(u64::from(delay.subsec_nanos() != 0))
        .max(1)
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value.split(',').any(|item| {
                let mut parameters = item.split(';');
                if !parameters
                    .next()
                    .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/html"))
                {
                    return false;
                }
                let mut quality = None;
                for parameter in parameters {
                    let Some((name, value)) = parameter.split_once('=') else {
                        return false;
                    };
                    if name.trim().eq_ignore_ascii_case("q") {
                        if quality.is_some() {
                            return false;
                        }
                        quality = Some(accept_quality_is_positive(value.trim()));
                    }
                }
                quality.unwrap_or(true)
            })
        })
}

fn accept_quality_is_positive(value: &str) -> bool {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match whole {
        "0" => fraction.bytes().any(|byte| byte != b'0'),
        "1" => fraction.bytes().all(|byte| byte == b'0'),
        _ => false,
    }
}

fn request_source_is_same_origin(
    headers: &HeaderMap,
    request_uri: &Uri,
    peer_ip: IpAddr,
    trusted_proxies: &[TrustedProxy],
) -> bool {
    let request_scheme =
        match trusted_forwarded_value(headers, "x-forwarded-proto", peer_ip, trusted_proxies) {
            Ok(Some(value)) => {
                if !(value.eq_ignore_ascii_case("http") || value.eq_ignore_ascii_case("https")) {
                    return false;
                }
                value
            }
            Err(()) => return false,
            Ok(None) => request_uri
                .scheme_str()
                // Dufs only accepts cleartext HTTP connections. A TLS gateway
                // must preserve the external scheme in X-Forwarded-Proto.
                .unwrap_or("http"),
        };
    let mode = if request_scheme.eq_ignore_ascii_case("https") {
        AdministratorOriginMode::ProductionHttps
    } else if request_scheme.eq_ignore_ascii_case("http") {
        AdministratorOriginMode::LoopbackDevelopmentHttp
    } else {
        return false;
    };
    let origin_values = raw_header_values(headers, ORIGIN_HEADER);
    let host_values = effective_host_values(headers, request_uri);
    let site_values = raw_header_values(headers, SEC_FETCH_SITE_HEADER);
    require_administrator_same_origin(mode, &origin_values, &host_values, &site_values).is_ok()
}

/// Preserve every raw field-line value for Foundation's singleton-header
/// policy. This adapter deliberately performs no selection or normalization.
fn raw_header_values<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a [u8]> {
    headers
        .get_all(name)
        .iter()
        .map(HeaderValue::as_bytes)
        .collect()
}

/// Treat HTTP/2 `:authority` (represented by `Uri::authority`) and every Host
/// field line as the same singleton input. Supplying both is ambiguous and is
/// intentionally rejected by Foundation as a duplicate.
fn effective_host_values<'a>(headers: &'a HeaderMap, request_uri: &'a Uri) -> Vec<&'a [u8]> {
    let mut values = raw_header_values(headers, HOST_HEADER);
    if let Some(authority) = request_uri.authority() {
        values.push(authority.as_str().as_bytes());
    }
    values
}

#[cfg(test)]
mod tests;
