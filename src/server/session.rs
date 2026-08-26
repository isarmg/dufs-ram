use super::{
    Request, Response, Server,
    problem::{ApiError, ErrorCode, render_problem},
    status_error,
};
use crate::{
    args::TrustedProxy,
    auth::{MAX_PASSWORD_BYTES, MAX_USERNAME_BYTES, session_token_from_cookie},
    http_utils::body_full,
    utils::{decode_hex_to_slice, encode_hex},
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
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::OwnedSemaphorePermit;

pub(super) const LOGIN_PATH: &str = "__dufs__/login";
pub(super) const LOGOUT_PATH: &str = "__dufs__/logout";
pub(super) const CSRF_HEADER: &str = "x-dufs-csrf-token";
pub(super) const LOGIN_ERROR_QUERY: &str = "login_error";

const LOGIN_HTML: &str = include_str!("../../assets/login.html");
const LOGIN_JS: &str = include_str!("../../assets/login.js");
const LOGIN_URI: &str = "/__dufs__/login";
const LOGIN_BODY_LIMIT: usize = 4 * 1024;
const LOGIN_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_BODY_GLOBAL_LIMIT: usize = 32;
const LOGIN_BODY_PER_IP_LIMIT: usize = 4;
const LOGIN_CSP: &str = "default-src 'none'; script-src 'sha256-C2dkZ9O8X30GXdSF3n4Y6gTZ7GA3ZZJzRs2D2Qdabqc='; style-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";
const LOGIN_ERROR_CAPACITY: usize = 1024;
const LOGIN_ERROR_TOKEN_BYTES: usize = 32;
const LOGIN_ERROR_TTL: Duration = Duration::from_secs(60);

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
    MissingFields,
    InvalidCredentials,
    TooManyRequests { retry_after_seconds: u64 },
}

impl LoginError {
    const fn message(self) -> &'static str {
        match self {
            Self::MissingFields => "Enter username and password.",
            Self::InvalidCredentials => "Invalid username or password.",
            Self::TooManyRequests { .. } => "Too many sign-in requests. Please try again later.",
        }
    }

    const fn status(self) -> StatusCode {
        match self {
            Self::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::MissingFields | Self::InvalidCredentials => StatusCode::OK,
        }
    }

    const fn retry_after_seconds(self) -> Option<u64> {
        match self {
            Self::TooManyRequests {
                retry_after_seconds,
            } => Some(retry_after_seconds),
            Self::MissingFields | Self::InvalidCredentials => None,
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
                return Ok(encode_hex(token));
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
        peer_ip: IpAddr,
        res: &mut Response,
    ) -> Result<Option<String>> {
        if !request_source_is_same_origin(
            req.headers(),
            req.uri(),
            peer_ip,
            &self.content.args.trusted_proxies,
        ) {
            status_error(res, StatusCode::FORBIDDEN, "Forbidden");
            self.add_private_security_headers(res);
            return Ok(None);
        }

        let client_ip = login_client_ip(req.headers(), peer_ip, &self.content.args.trusted_proxies);
        if let Err(delay) = self.admission.login_rate_limiter.check_request(client_ip) {
            self.redirect_login_error(
                res,
                LoginError::TooManyRequests {
                    retry_after_seconds: retry_after_seconds(delay),
                },
            )?;
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

        let Some(body_permit) = self.admission.login_body_admission.try_acquire(client_ip) else {
            self.redirect_login_error(
                res,
                LoginError::TooManyRequests {
                    retry_after_seconds: 1,
                },
            )?;
            return Ok(None);
        };
        let previous_token = req
            .headers()
            .get(hyper::header::COOKIE)
            .and_then(session_token_from_cookie)
            .map(str::to_owned);
        let body = match tokio::time::timeout(
            LOGIN_BODY_TIMEOUT,
            Limited::new(req.into_body(), LOGIN_BODY_LIMIT).collect(),
        )
        .await
        {
            Ok(Ok(body)) => body.to_bytes(),
            Ok(Err(err)) => {
                if err.downcast_ref::<LengthLimitError>().is_some() {
                    status_error(res, StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
                } else {
                    status_error(res, StatusCode::BAD_REQUEST, "Invalid request body");
                }
                self.add_private_security_headers(res);
                return Ok(None);
            }
            Err(_) => {
                status_error(
                    res,
                    StatusCode::REQUEST_TIMEOUT,
                    "Login request body timed out",
                );
                self.add_private_security_headers(res);
                return Ok(None);
            }
        };
        drop(body_permit);

        let Some((username, password)) = parse_login_form(&body) else {
            self.redirect_login_error(res, LoginError::InvalidCredentials)?;
            return Ok(None);
        };
        if username.is_empty() || password.is_empty() {
            self.redirect_login_error(res, LoginError::MissingFields)?;
            return Ok(None);
        }

        if let Err(delay) = self
            .admission
            .login_rate_limiter
            .check_account_backoff(client_ip, &username)
        {
            self.redirect_login_error(
                res,
                LoginError::TooManyRequests {
                    retry_after_seconds: retry_after_seconds(delay),
                },
            )?;
            return Ok(None);
        }

        let Ok(permit) = self.admission.login_slots.clone().try_acquire_owned() else {
            self.redirect_login_error(
                res,
                LoginError::TooManyRequests {
                    retry_after_seconds: 1,
                },
            )?;
            return Ok(None);
        };

        let auth = self.content.auth.clone();
        let login_user = username.clone();
        let verified_user = run_with_login_slot(permit, move || {
            auth.verify_credentials(&login_user, &password)
        })
        .await?;

        let Some(verified_user) = verified_user else {
            self.admission
                .login_rate_limiter
                .record_failure(client_ip, &username);
            self.redirect_login_error(res, LoginError::InvalidCredentials)?;
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
        res.headers_mut()
            .insert(LOCATION, HeaderValue::from_static("/"));
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
            self.admission
                .login_errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .consume(token)
        });
        let status = error.map_or(StatusCode::OK, LoginError::status);
        self.send_login_page(res, status, error.map(LoginError::message))?;
        if let Some(retry_after_seconds) = error.and_then(LoginError::retry_after_seconds) {
            res.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&retry_after_seconds.to_string())?,
            );
        }
        Ok(())
    }

    fn send_login_page(
        &self,
        res: &mut Response,
        status: StatusCode,
        error: Option<&str>,
    ) -> Result<()> {
        let max_password_bytes = MAX_PASSWORD_BYTES.to_string();
        let output = LOGIN_HTML
            .replace("__LOGIN_ACTION__", LOGIN_URI)
            .replace("__ASSETS_PREFIX__", &self.content.assets_prefix)
            .replace("__LOGIN_SCRIPT__", LOGIN_JS)
            .replace("__MAX_PASSWORD_BYTES__", &max_password_bytes)
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
            .admission
            .login_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(error)?;
        let location = format!("{LOGIN_URI}?{LOGIN_ERROR_QUERY}={token}");
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
        api_request: bool,
        res: &mut Response,
    ) -> Result<()> {
        res.headers_mut()
            .insert(SET_COOKIE, self.content.auth.clear_session_cookie());
        if !api_request && matches!(*method, Method::GET | Method::HEAD) && accepts_html(headers) {
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
        self.add_private_security_headers(res);
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
        headers
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|candidate| {
                self.content
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

fn decode_login_error_token(encoded_token: &str) -> Option<[u8; LOGIN_ERROR_TOKEN_BYTES]> {
    if encoded_token.len() != LOGIN_ERROR_TOKEN_BYTES * 2 {
        return None;
    }
    let mut token = [0u8; LOGIN_ERROR_TOKEN_BYTES];
    decode_hex_to_slice(encoded_token, &mut token).then_some(())?;
    Some(token)
}

fn retry_after_seconds(delay: Duration) -> u64 {
    delay
        .as_secs()
        .saturating_add(u64::from(delay.subsec_nanos() != 0))
        .max(1)
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
    if username.len() > MAX_USERNAME_BYTES
        || password.len() > MAX_PASSWORD_BYTES
        || !values.is_empty()
    {
        return None;
    }
    Some((username, password))
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
    let Some(origin_scheme) = origin.scheme_str() else {
        return false;
    };
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
    origin_scheme.eq_ignore_ascii_case(request_scheme)
        && origin
            .authority()
            .is_some_and(|authority| authority.as_str().eq_ignore_ascii_case(request_authority))
}

#[cfg(test)]
mod tests;
