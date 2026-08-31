use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hyper::header::HOST;
use sarmg_admin_auth::require_single_csrf_token;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::{Semaphore, oneshot};

fn trusted_proxies(values: &[&str]) -> Vec<TrustedProxy> {
    values.iter().map(|value| value.parse().unwrap()).collect()
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
    assert!(LOGIN_CSP.contains("connect-src 'self'"));
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

    for malformed in ["text/html;q", "text/html; q", "text/html;level"] {
        headers.insert(ACCEPT, HeaderValue::from_static(malformed));
        assert!(
            !accepts_html(&headers),
            "malformed media parameter was accepted: {malformed}"
        );
    }

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
    let request_uri = ADMIN_LOGIN_PATH.parse::<Uri>().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("files.example"));
    headers.insert("origin", HeaderValue::from_static("https://files.example"));
    headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));

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
    headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
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
fn security_sensitive_origin_headers_reject_duplicate_lines() {
    let peer = "127.0.0.1".parse().unwrap();
    let proxies = trusted_proxies(&["127.0.0.1/32"]);
    for name in [HOST.as_str(), "origin", "sec-fetch-site"] {
        let mut headers = HeaderMap::new();
        headers.append(HOST, HeaderValue::from_static("files.example.test"));
        headers.append(
            "origin",
            HeaderValue::from_static("https://files.example.test"),
        );
        headers.append("sec-fetch-site", HeaderValue::from_static("same-origin"));
        headers.append("x-forwarded-proto", HeaderValue::from_static("https"));
        let duplicate = headers
            .get(name)
            .expect("baseline header is present")
            .clone();
        headers.append(name, duplicate);
        assert!(!request_source_is_same_origin(
            &headers,
            &Uri::from_static("/api/v2/auth/login"),
            peer,
            &proxies,
        ));
    }
}

#[test]
fn csrf_rejects_duplicate_header_lines_before_token_verification() {
    let token = HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let mut headers = HeaderMap::new();
    headers.append(CSRF_HEADER, token.clone());
    let values = raw_header_values(&headers, CSRF_HEADER);
    assert_eq!(
        require_single_csrf_token(&values).unwrap(),
        token.to_str().unwrap()
    );
    headers.append(CSRF_HEADER, token);
    let values = raw_header_values(&headers, CSRF_HEADER);
    assert!(require_single_csrf_token(&values).is_err());
}

#[test]
fn opaque_origin_is_never_a_current_administrator_origin() {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("localhost:5000"));
    headers.insert("origin", HeaderValue::from_static("null"));
    headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    let uri = Uri::from_static("http://localhost:5000/api/v2/auth/login");
    assert!(!request_source_is_same_origin(
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

#[test]
fn direct_http_is_limited_to_complete_loopback_browser_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("127.0.0.1:5000"));
    headers.insert("origin", HeaderValue::from_static("http://127.0.0.1:5000"));
    headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    assert!(request_source_is_same_origin(
        &headers,
        &Uri::from_static("/api/v2/auth/login"),
        "127.0.0.1".parse().unwrap(),
        &[],
    ));

    headers.remove(HOST);
    let authority_uri = Uri::from_static("http://127.0.0.1:5000/api/v2/auth/login");
    assert!(request_source_is_same_origin(
        &headers,
        &authority_uri,
        "127.0.0.1".parse().unwrap(),
        &[],
    ));
    headers.insert(HOST, HeaderValue::from_static("127.0.0.1:5000"));
    assert!(!request_source_is_same_origin(
        &headers,
        &authority_uri,
        "127.0.0.1".parse().unwrap(),
        &[],
    ));

    headers.remove(HOST);
    headers.remove("sec-fetch-site");
    assert!(!request_source_is_same_origin(
        &headers,
        &authority_uri,
        "127.0.0.1".parse().unwrap(),
        &[],
    ));
}
