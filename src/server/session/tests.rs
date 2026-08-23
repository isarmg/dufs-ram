use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::{Semaphore, oneshot};

fn trusted_proxies(values: &[&str]) -> Vec<TrustedProxy> {
    values.iter().map(|value| value.parse().unwrap()).collect()
}

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

    for password in [
        "p".repeat(MAX_PASSWORD_BYTES),
        "é".repeat(MAX_PASSWORD_BYTES / "é".len()),
    ] {
        let form = form_urlencoded::Serializer::new(String::new())
            .append_pair("username", "user")
            .append_pair("password", &password)
            .finish();
        assert_eq!(
            parse_login_form(form.as_bytes()),
            Some(("user".to_string(), password))
        );
    }
    for password in [
        "p".repeat(MAX_PASSWORD_BYTES + 1),
        format!("a{}", "é".repeat(MAX_PASSWORD_BYTES / "é".len())),
    ] {
        let form = form_urlencoded::Serializer::new(String::new())
            .append_pair("username", "user")
            .append_pair("password", &password)
            .finish();
        assert!(parse_login_form(form.as_bytes()).is_none());
    }
}

#[test]
fn login_script_and_csp_enforce_the_shared_password_byte_limit() {
    assert!(LOGIN_HTML.contains(r#"data-max-bytes="__MAX_PASSWORD_BYTES__""#));
    assert!(!LOGIN_HTML.contains(r#"type="password" maxlength="#));
    assert_eq!(LOGIN_HTML.matches("__LOGIN_SCRIPT__").count(), 1);
    assert!(LOGIN_JS.contains("TextEncoder"));
    assert!(LOGIN_JS.contains("setCustomValidity"));
    assert!(LOGIN_HTML.contains(r#"href="/__ASSETS_PREFIX__login.css""#));
    assert!(!LOGIN_HTML.contains("<style>"));
    assert!(LOGIN_CSP.contains("style-src 'self'"));
    assert!(!LOGIN_CSP.contains("'unsafe-inline'"));

    let digest = Sha256::digest(LOGIN_JS.as_bytes());
    let source = format!("'sha256-{}'", STANDARD.encode(digest));
    let expected_directive = format!("script-src {source}");
    assert_eq!(
        LOGIN_CSP
            .split(';')
            .map(str::trim)
            .find(|directive| directive.starts_with("script-src ")),
        Some(expected_directive.as_str()),
        "login CSP must authorize exactly the embedded validation script"
    );
}

#[test]
fn retry_after_rounds_remaining_time_up_to_whole_seconds() {
    assert_eq!(retry_after_seconds(Duration::ZERO), 1);
    assert_eq!(retry_after_seconds(Duration::from_secs(1)), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(1_500)), 2);
    assert_eq!(retry_after_seconds(Duration::from_millis(1_999)), 2);
    assert_eq!(retry_after_seconds(Duration::from_secs(60)), 60);
}

#[test]
fn html_acceptance_requires_an_exact_positive_media_range() {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("text/html;q=0, application/json"),
    );
    assert!(!accepts_html(&headers));

    headers.insert(ACCEPT, HeaderValue::from_static("text/htmlx"));
    assert!(!accepts_html(&headers));

    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/html; q=0.001"),
    );
    assert!(accepts_html(&headers));

    headers.remove(ACCEPT);
    headers.append(ACCEPT, HeaderValue::from_static("application/json"));
    headers.append(
        ACCEPT,
        HeaderValue::from_static("text/html;level=1;q=1.000"),
    );
    assert!(accepts_html(&headers));
}

#[test]
fn same_origin_requires_matching_external_scheme_and_authority() {
    let request_uri = "/__dufs__/login".parse::<Uri>().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("files.example"));
    headers.insert("origin", HeaderValue::from_static("https://files.example"));

    assert!(!request_source_is_same_origin(
        &headers,
        &request_uri,
        "127.0.0.1".parse().unwrap(),
        &[],
    ));
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    assert!(!request_source_is_same_origin(
        &headers,
        &request_uri,
        "127.0.0.1".parse().unwrap(),
        &[],
    ));
    let loopback_proxy = trusted_proxies(&["127.0.0.1/32"]);
    assert!(request_source_is_same_origin(
        &headers,
        &request_uri,
        "127.0.0.1".parse().unwrap(),
        &loopback_proxy,
    ));
    assert!(
        !request_source_is_same_origin(
            &headers,
            &request_uri,
            "198.51.100.7".parse().unwrap(),
            &loopback_proxy,
        ),
        "an untrusted peer controlled the external request scheme"
    );
    let remote_proxy = trusted_proxies(&["198.51.100.0/24"]);
    assert!(request_source_is_same_origin(
        &headers,
        &request_uri,
        "198.51.100.7".parse().unwrap(),
        &remote_proxy,
    ));

    headers.insert("origin", HeaderValue::from_static("http://files.example"));
    assert!(!request_source_is_same_origin(
        &headers,
        &request_uri,
        "127.0.0.1".parse().unwrap(),
        &loopback_proxy,
    ));
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
    assert!(!request_source_is_same_origin(
        &headers,
        &request_uri,
        "127.0.0.1".parse().unwrap(),
        &loopback_proxy,
    ));
}

#[test]
fn forwarded_login_address_requires_an_explicit_trusted_proxy() {
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.10"));
    assert_eq!(
        login_client_ip(&headers, "127.0.0.1".parse().unwrap(), &[]),
        "127.0.0.1".parse::<IpAddr>().unwrap()
    );

    let loopback_proxy = trusted_proxies(&["127.0.0.1/32"]);
    assert_eq!(
        login_client_ip(&headers, "127.0.0.1".parse().unwrap(), &loopback_proxy,),
        "192.0.2.10".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        login_client_ip(&headers, "198.51.100.7".parse().unwrap(), &loopback_proxy,),
        "198.51.100.7".parse::<IpAddr>().unwrap()
    );
    let remote_proxy = trusted_proxies(&["198.51.100.0/24"]);
    assert_eq!(
        login_client_ip(&headers, "198.51.100.7".parse().unwrap(), &remote_proxy,),
        "192.0.2.10".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        login_client_ip(
            &headers,
            "::ffff:127.0.0.1".parse().unwrap(),
            &loopback_proxy,
        ),
        "192.0.2.10".parse::<IpAddr>().unwrap()
    );
    headers.insert(
        "x-forwarded-for",
        HeaderValue::from_static("192.0.2.10, 127.0.0.1"),
    );
    assert_eq!(
        login_client_ip(&headers, "127.0.0.1".parse().unwrap(), &loopback_proxy,),
        "127.0.0.1".parse::<IpAddr>().unwrap()
    );
    headers.remove("x-forwarded-for");
    headers.append("x-forwarded-for", HeaderValue::from_static("192.0.2.10"));
    headers.append("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));
    assert_eq!(
        login_client_ip(&headers, "127.0.0.1".parse().unwrap(), &loopback_proxy,),
        "127.0.0.1".parse::<IpAddr>().unwrap()
    );
}

#[test]
fn login_body_admission_enforces_global_and_per_ip_limits() {
    let admission = LoginBodyAdmission::new(2, 1);
    let first_ip = "192.0.2.1".parse().unwrap();
    let second_ip = "192.0.2.2".parse().unwrap();
    let third_ip = "192.0.2.3".parse().unwrap();

    let first = admission
        .try_acquire(first_ip)
        .expect("first client should be admitted");
    assert!(admission.try_acquire(first_ip).is_none());
    let second = admission
        .try_acquire(second_ip)
        .expect("second client should fill the global capacity");
    assert!(admission.try_acquire(third_ip).is_none());

    drop(first);
    let third = admission
        .try_acquire(third_ip)
        .expect("dropping a permit should release both counters");
    drop((second, third));
    let state = admission
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.total, 0);
    assert!(state.per_ip.is_empty());
}

#[test]
fn login_error_tokens_are_random_and_consumed_once() -> Result<()> {
    let mut store = LoginErrorStore::default();
    let missing = store.insert(LoginError::MissingFields)?;
    let invalid = store.insert(LoginError::InvalidCredentials)?;
    let rate_limited = store.insert(LoginError::TooManyRequests {
        retry_after_seconds: 7,
    })?;
    assert_eq!(missing.len(), LOGIN_ERROR_TOKEN_BYTES * 2);
    assert_ne!(missing, invalid);
    assert_ne!(missing, rate_limited);
    assert_eq!(store.consume(&missing), Some(LoginError::MissingFields));
    assert_eq!(store.consume(&missing), None);
    assert_eq!(
        store.consume(&invalid),
        Some(LoginError::InvalidCredentials)
    );
    assert_eq!(
        store.consume(&rate_limited),
        Some(LoginError::TooManyRequests {
            retry_after_seconds: 7,
        })
    );
    assert_eq!(store.consume("untrusted-text"), None);

    let expired_token = [0xA5; LOGIN_ERROR_TOKEN_BYTES];
    store.entries.insert(
        expired_token,
        LoginErrorRecord {
            error: LoginError::TooManyRequests {
                retry_after_seconds: 9,
            },
            created_at: Instant::now()
                .checked_sub(LOGIN_ERROR_TTL)
                .ok_or_else(|| anyhow!("Could not construct expired test instant"))?,
        },
    );
    assert_eq!(store.consume(&encode_hex(expired_token)), None);

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
        &Uri::from_static("/"),
        "127.0.0.1".parse().unwrap(),
        &[],
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
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    let loopback_proxy = trusted_proxies(&["127.0.0.1/32"]);
    assert!(request_source_is_same_origin(
        &headers,
        &Uri::from_static("/"),
        "127.0.0.1".parse().unwrap(),
        &loopback_proxy,
    ));
    headers.insert(
        "origin",
        HeaderValue::from_static("https://evil.example.test"),
    );
    assert!(!request_source_is_same_origin(
        &headers,
        &Uri::from_static("/"),
        "127.0.0.1".parse().unwrap(),
        &loopback_proxy,
    ));
}

#[test]
fn opaque_origin_requires_browser_same_origin_metadata() {
    let mut headers = HeaderMap::new();
    headers.insert("origin", HeaderValue::from_static("null"));
    headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    let uri = Uri::from_static("https://localhost:5000/__dufs__/login");
    assert!(request_source_is_same_origin(
        &headers,
        &uri,
        "127.0.0.1".parse().unwrap(),
        &[],
    ));

    headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
    assert!(!request_source_is_same_origin(
        &headers,
        &uri,
        "127.0.0.1".parse().unwrap(),
        &[],
    ));
}
