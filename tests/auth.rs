#[path = "support/fixtures.rs"]
mod fixtures;

use fixtures::{
    ADMIN_ACCOUNT, Error, TEST_ACCOUNT, TEST_PASSWORD, TestServer, TestSession, USER_ACCOUNT,
    dufs_command, preflight_upload_target_with, server, tmpdir, with_new_upload_headers,
    with_new_upload_overwrite_headers, with_resume_upload_headers, with_upload_headers,
};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, COOKIE, LOCATION, RETRY_AFTER, SET_COOKIE,
};
use reqwest::{Method, StatusCode, Url};
use rstest::rstest;
use sarmg_contracts::{AdministratorRole, AdministratorSession, ErrorEnvelope};
use serde_json::{Value, json};
use uuid::Uuid;

const CSRF_HEADER: &str = "x-csrf-token";
const SESSION_COOKIE_NAME: &str = "__Host-dufs-session";

fn client_without_redirects() -> Result<Client, Error> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn login_response(server: &TestServer, username: &str, password: &str) -> Result<Response, Error> {
    login_response_from(server, username, password, None)
}

fn login_response_from(
    server: &TestServer,
    username: &str,
    password: &str,
    client_ip: Option<&str>,
) -> Result<Response, Error> {
    let request = client_without_redirects()?
        .post(server.url().join("api/v2/auth/login")?)
        .header("origin", server.url().origin().ascii_serialization())
        .header("sec-fetch-site", "same-origin")
        .header(CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&json!({
            "username": username,
            "password": password,
        }))?);
    let request = match client_ip {
        Some(client_ip) => request.header("x-forwarded-for", client_ip),
        None => request,
    };
    Ok(request.send()?)
}

fn request_with_csrf(
    server: &TestServer,
    session: &TestSession,
    method: Method,
    url: Url,
    csrf: Option<&str>,
) -> RequestBuilder {
    let request = server
        .raw_request(method, url)
        .header(COOKIE, session.cookie())
        .header("origin", server.url().origin().ascii_serialization())
        .header("sec-fetch-site", "same-origin");
    match csrf {
        Some(csrf) => request.header(CSRF_HEADER, csrf),
        None => request,
    }
}

#[rstest]
fn unauthenticated_html_navigation_redirects_to_login(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let client = client_without_redirects()?;
    let response = client
        .get(server.url())
        .header(ACCEPT, "text/html")
        .send()?;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/__dufs__/login")
    );
    assert!(!response.headers().contains_key("www-authenticate"));

    let login_page = client.get(server.url().join("__dufs__/login")?).send()?;
    assert_eq!(login_page.status(), StatusCode::OK);
    assert!(
        login_page
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("script-src 'sha256-"))
    );
    assert!(
        login_page
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.contains("style-src 'self'")
                    && value.contains("connect-src 'self'")
                    && !value.contains("'unsafe-inline'")
            })
    );
    let body = login_page.text()?;
    assert!(body.contains("<form"));
    assert!(body.contains("class=\"content-card login-card\""));
    assert!(body.contains("rel=\"stylesheet\""));
    assert!(body.contains("/__dufs_assets_"));
    assert!(body.contains("/login.css"));
    assert!(!body.contains("<style>"));

    let stylesheet_path = body
        .split_once("rel=\"stylesheet\" href=\"")
        .and_then(|(_, rest)| rest.split_once('"').map(|(path, _)| path))
        .ok_or("Login page is missing its stylesheet URL")?;
    let stylesheet = client.get(server.url().join(stylesheet_path)?).send()?;
    assert_eq!(stylesheet.status(), StatusCode::OK);
    assert_eq!(
        stylesheet
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/css; charset=UTF-8")
    );
    assert_eq!(
        stylesheet
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    assert!(body.contains("novalidate"));
    assert_eq!(body.matches(" required").count(), 2);
    assert!(body.contains("name=\"username\""));
    assert!(body.contains("name=\"password\""));
    assert!(body.contains("maxlength=\"64\""));
    assert!(body.contains("data-min-bytes=\"12\""));
    assert!(body.contains("data-max-bytes=\"1024\""));
    assert!(!body.contains("__MIN_PASSWORD_BYTES__"));
    assert!(!body.contains("__MAX_PASSWORD_BYTES__"));
    Ok(())
}

#[rstest]
fn noncanonical_login_paths_are_rejected(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let client = client_without_redirects()?;
    for raw_path in [
        "/__dufs__/login/",
        "//__dufs__/login",
        "/%5F%5Fdufs%5F%5F/login",
    ] {
        let url = Url::parse(&format!("http://localhost:{}{raw_path}", server.port()))?;
        assert_eq!(url.path(), raw_path);
        let response = client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&json!({
                "username": "user",
                "password": TEST_PASSWORD,
            }))?)
            .send()?;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "path={raw_path}"
        );
        assert!(!response.headers().contains_key(SET_COOKIE));
    }
    Ok(())
}

#[rstest]
fn unauthenticated_writes_return_401_before_touching_disk(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let put_path = server.path().join("unauthenticated-put.txt");
    let patch_path = server.path().join("unauthenticated-patch.txt");
    let delete_path = server.path().join("unauthenticated-delete.txt");
    std::fs::write(&patch_path, "original patch")?;
    std::fs::write(&delete_path, "original delete")?;

    let put = with_new_upload_headers(
        server.raw_request(Method::PUT, server.url().join("unauthenticated-put.txt")?),
        "must not be written".len() as u64,
    )
    .body("must not be written")
    .send()?;
    assert_eq!(put.status(), StatusCode::UNAUTHORIZED);

    let patch = with_resume_upload_headers(
        server.raw_request(
            Method::PATCH,
            server.url().join("unauthenticated-patch.txt")?,
        ),
        Uuid::new_v4(),
        "original patch must not be appended".len() as u64,
        "original patch".len() as u64,
    )
    .body(" must not be appended")
    .send()?;
    assert_eq!(patch.status(), StatusCode::UNAUTHORIZED);

    let delete = server
        .raw_request(
            Method::DELETE,
            server.url().join("unauthenticated-delete.txt")?,
        )
        .send()?;
    assert_eq!(delete.status(), StatusCode::UNAUTHORIZED);

    let post = server
        .raw_request(Method::POST, server.url().join("__dufs__/api/mkdir")?)
        .header(CONTENT_TYPE, "application/json")
        .body(json!({"path": "/unauthenticated-directory"}).to_string())
        .send()?;
    assert_eq!(post.status(), StatusCode::UNAUTHORIZED);

    assert!(!put_path.exists());
    assert_eq!(std::fs::read_to_string(patch_path)?, "original patch");
    assert_eq!(std::fs::read_to_string(delete_path)?, "original delete");
    assert!(!server.path().join("unauthenticated-directory").exists());
    Ok(())
}

#[rstest]
fn administrator_login_uses_exact_foundation_json_and_sets_cookie_only_on_success(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    for (username, password, expected_status, expected_code) in [
        ("", "", StatusCode::BAD_REQUEST, "invalid_request_body"),
        (
            "user",
            "short",
            StatusCode::BAD_REQUEST,
            "invalid_request_body",
        ),
        (
            "invalid@username",
            TEST_PASSWORD,
            StatusCode::BAD_REQUEST,
            "invalid_request_body",
        ),
        (
            "user",
            "wrong-password",
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
        ),
    ] {
        let rejected = login_response(&server, username, password)?;
        assert_eq!(rejected.status(), expected_status);
        assert!(!rejected.headers().contains_key(SET_COOKIE));
        assert!(!rejected.headers().contains_key(RETRY_AFTER));
        let envelope: ErrorEnvelope = serde_json::from_str(&rejected.text()?)?;
        assert_eq!(envelope.code.as_str(), expected_code);
        assert!(!envelope.retryable);
        assert!(envelope.details.is_empty());
    }

    for invalid_shape in [
        json!({ "password": TEST_PASSWORD }),
        json!({ "username": "user", "password": TEST_PASSWORD, "extra": true }),
    ] {
        let rejected = client_without_redirects()?
            .post(server.url().join("api/v2/auth/login")?)
            .header("origin", server.url().origin().ascii_serialization())
            .header("sec-fetch-site", "same-origin")
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&invalid_shape)?)
            .send()?;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(!rejected.headers().contains_key(SET_COOKIE));
        let envelope: ErrorEnvelope = serde_json::from_str(&rejected.text()?)?;
        assert_eq!(envelope.code.as_str(), "invalid_request_body");
    }

    let accepted = login_response(&server, " User ", TEST_PASSWORD)?;
    assert_eq!(accepted.status(), StatusCode::OK);
    let cookie = accepted
        .headers()
        .get(SET_COOKIE)
        .ok_or("Successful login did not set a cookie")?
        .to_str()?;
    let cookie_lower = cookie.to_ascii_lowercase();
    assert!(cookie.starts_with(&format!("{SESSION_COOKIE_NAME}=")));
    for attribute in ["path=/", "httponly", "secure", "samesite=strict"] {
        assert!(cookie_lower.contains(attribute), "cookie={cookie}");
    }
    assert!(!cookie_lower.contains("domain="));
    let body = accepted.text()?;
    let raw: Value = serde_json::from_str(&body)?;
    let keys = raw
        .as_object()
        .ok_or("Login response is not an object")?
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        ["authenticated", "csrf_token", "role", "user_id", "username"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    let session: AdministratorSession = serde_json::from_str(&body)?;
    assert_eq!(session.username, "user");
    assert_eq!(session.role, AdministratorRole::Admin);
    assert_eq!(session.csrf_token.len(), 43);
    assert!(
        session
            .csrf_token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    );

    let removed_form_endpoint = client_without_redirects()?
        .post(server.url().join("__dufs__/login")?)
        .header(CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&json!({
            "username": "user",
            "password": TEST_PASSWORD,
        }))?)
        .send()?;
    assert_eq!(
        removed_form_endpoint.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert!(!removed_form_endpoint.headers().contains_key(SET_COOKIE));
    Ok(())
}

#[rstest]
fn administrator_session_and_auth_failures_use_the_foundation_contract(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let session = server.login("user", TEST_PASSWORD)?;
    let restored = server
        .raw_request(Method::GET, server.url().join("api/v2/auth/session")?)
        .header(COOKIE, session.cookie())
        .send()?;
    assert_eq!(restored.status(), StatusCode::OK);
    let restored: AdministratorSession = serde_json::from_str(&restored.text()?)?;
    assert_eq!(restored.username, "user");
    assert_eq!(restored.role, AdministratorRole::Admin);
    assert_eq!(restored.csrf_token, session.csrf_token());

    let unauthenticated = server
        .raw_request(Method::GET, server.url().join("api/v2/auth/session")?)
        .send()?;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let unauthenticated: ErrorEnvelope = serde_json::from_str(&unauthenticated.text()?)?;
    assert_eq!(unauthenticated.code.as_str(), "unauthorized");
    assert!(!unauthenticated.retryable);
    assert!(unauthenticated.details.is_empty());

    let missing_csrf = server
        .raw_request(Method::POST, server.url().join("api/v2/auth/logout")?)
        .header(COOKIE, session.cookie())
        .header("origin", server.url().origin().ascii_serialization())
        .header("sec-fetch-site", "same-origin")
        .send()?;
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);
    let missing_csrf: ErrorEnvelope = serde_json::from_str(&missing_csrf.text()?)?;
    assert_eq!(missing_csrf.code.as_str(), "csrf_failed");
    assert!(!missing_csrf.retryable);
    assert!(missing_csrf.details.is_empty());
    Ok(())
}

#[rstest]
fn administrator_login_rejects_ambiguous_session_cookie_without_rotation(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let token = "A".repeat(43);
    let response = client_without_redirects()?
        .post(server.url().join("api/v2/auth/login")?)
        .header("origin", server.url().origin().ascii_serialization())
        .header("sec-fetch-site", "same-origin")
        .header(CONTENT_TYPE, "application/json")
        .header(
            COOKIE,
            format!("{SESSION_COOKIE_NAME}={token}; {SESSION_COOKIE_NAME}={token}"),
        )
        .body(serde_json::to_vec(&json!({
            "username": "user",
            "password": TEST_PASSWORD,
        }))?)
        .send()?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.headers().contains_key(SET_COOKIE));
    let envelope: ErrorEnvelope = serde_json::from_str(&response.text()?)?;
    assert_eq!(envelope.code.as_str(), "invalid_cookie_header");
    assert!(!envelope.retryable);
    assert!(envelope.details.is_empty());
    Ok(())
}

#[rstest]
fn login_backoff_is_scoped_to_the_source_and_account_pair(
    #[with(&[
        "--trusted-proxy",
        "127.0.0.1/32"
    ], &[USER_ACCOUNT])]
    server: TestServer,
) -> Result<(), Error> {
    let first_ip = "192.0.2.10";
    let second_ip = "192.0.2.11";

    for _ in 0..5 {
        let rejected = login_response_from(&server, "user", "wrong-password", Some(first_ip))?;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        assert!(!rejected.headers().contains_key(RETRY_AFTER));
        assert!(!rejected.headers().contains_key(SET_COOKIE));
    }

    let blocked = login_response_from(&server, "user", TEST_PASSWORD, Some(first_ip))?;
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!blocked.headers().contains_key(SET_COOKIE));
    assert_eq!(
        blocked
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );
    let blocked_envelope: ErrorEnvelope = serde_json::from_str(&blocked.text()?)?;
    assert_eq!(blocked_envelope.code.as_str(), "login_rate_limited");
    assert!(blocked_envelope.retryable);
    assert_eq!(blocked_envelope.details["retry_after"], 1);

    let other_source = login_response_from(&server, "user", TEST_PASSWORD, Some(second_ip))?;
    assert_eq!(other_source.status(), StatusCode::OK);
    assert!(other_source.headers().contains_key(SET_COOKIE));
    assert!(!other_source.headers().contains_key(RETRY_AFTER));

    std::thread::sleep(std::time::Duration::from_millis(1_050));
    let next_failure = login_response_from(&server, "user", "wrong-password", Some(first_ip))?;
    assert_eq!(next_failure.status(), StatusCode::UNAUTHORIZED);
    assert!(!next_failure.headers().contains_key(RETRY_AFTER));

    let longer_backoff = login_response_from(&server, "user", TEST_PASSWORD, Some(first_ip))?;
    assert_eq!(longer_backoff.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        longer_backoff
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert!(!longer_backoff.headers().contains_key(SET_COOKIE));
    Ok(())
}

#[rstest]
fn successful_login_clears_the_pair_failure_history(
    #[with(&[
        "--trusted-proxy",
        "127.0.0.1/32"
    ], &[USER_ACCOUNT])]
    server: TestServer,
) -> Result<(), Error> {
    let client_ip = "192.0.2.20";
    for _ in 0..4 {
        let rejected = login_response_from(&server, "user", "wrong-password", Some(client_ip))?;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        assert!(!rejected.headers().contains_key(RETRY_AFTER));
    }

    let first_success = login_response_from(&server, "user", TEST_PASSWORD, Some(client_ip))?;
    assert_eq!(first_success.status(), StatusCode::OK);
    assert!(first_success.headers().contains_key(SET_COOKIE));

    let failure_after_success =
        login_response_from(&server, "user", "wrong-password", Some(client_ip))?;
    assert_eq!(failure_after_success.status(), StatusCode::UNAUTHORIZED);
    assert!(!failure_after_success.headers().contains_key(RETRY_AFTER));

    let second_success = login_response_from(&server, "user", TEST_PASSWORD, Some(client_ip))?;
    assert_eq!(second_success.status(), StatusCode::OK);
    assert!(second_success.headers().contains_key(SET_COOKIE));
    assert!(!second_success.headers().contains_key(RETRY_AFTER));
    Ok(())
}

#[rstest]
fn untrusted_forwarded_addresses_do_not_split_login_backoff(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    for _ in 0..5 {
        let rejected = login_response_from(&server, "user", "wrong-password", Some("192.0.2.30"))?;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        assert!(!rejected.headers().contains_key(SET_COOKIE));
    }

    let blocked = login_response_from(&server, "user", TEST_PASSWORD, Some("192.0.2.31"))?;
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!blocked.headers().contains_key(SET_COOKIE));
    assert!(blocked.headers().contains_key(RETRY_AFTER));
    Ok(())
}

#[rstest]
fn cross_site_login_and_write_requests_are_rejected(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let login = client_without_redirects()?
        .post(server.url().join("api/v2/auth/login")?)
        .header(CONTENT_TYPE, "application/json")
        .header("origin", "https://evil.example")
        .header("sec-fetch-site", "cross-site")
        .body(serde_json::to_vec(&json!({
            "username": "user",
            "password": TEST_PASSWORD,
        }))?)
        .send()?;
    assert_eq!(login.status(), StatusCode::FORBIDDEN);
    assert!(!login.headers().contains_key(SET_COOKIE));

    let session = server.login("user", TEST_PASSWORD)?;
    let target = server.url().join("cross-site-write.txt")?;
    let write = with_new_upload_headers(
        server.request_with(&session, Method::PUT, target),
        "blocked".len() as u64,
    )
    .header("origin", "https://evil.example")
    .header("sec-fetch-site", "cross-site")
    .body("blocked")
    .send()?;
    assert_eq!(write.status(), StatusCode::FORBIDDEN);
    assert!(!server.path().join("cross-site-write.txt").exists());
    Ok(())
}

#[test]
fn configured_argon2id_account_can_log_in() -> Result<(), Error> {
    let server = server(&[] as &[&str], &[USER_ACCOUNT]);

    let session = server.login("user", TEST_PASSWORD)?;
    assert_eq!(
        server.get_with(&session, server.url())?.status(),
        StatusCode::OK
    );
    Ok(())
}

#[rstest]
fn running_server_argv_contains_only_the_protected_config_path(
    server: TestServer,
) -> Result<(), Error> {
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", server.process_id()))?;
    assert!(
        !cmdline
            .windows(b"$argon2id$".len())
            .any(|window| window == b"$argon2id$"),
        "running server argv exposed an Argon2id PHC"
    );
    assert!(
        !cmdline
            .windows(TEST_ACCOUNT.len())
            .any(|window| window == TEST_ACCOUNT.as_bytes()),
        "running server argv exposed the complete test account"
    );
    let argv = cmdline
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .collect::<Vec<_>>();
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == b"--config" && !pair[1].is_empty()),
        "running server argv did not contain the protected config path"
    );
    Ok(())
}

#[rstest]
fn invalid_password_hash_is_rejected_at_startup(
    tmpdir: assert_fs::fixture::TempDir,
) -> Result<(), Error> {
    let (mut command, _auth_config) = dufs_command(&["user:not-an-argon2id-phc"]);
    let output = command.arg(tmpdir.path()).output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid Argon2id PHC in auth account #1"),
        "stderr={stderr}"
    );
    Ok(())
}

#[rstest]
fn accounts_have_independent_sessions_and_full_filesystem_access(
    #[with(&[] as &[&str], &[ADMIN_ACCOUNT, USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let admin = server.login("admin", TEST_PASSWORD)?;
    let user = server.login("user", TEST_PASSWORD)?;
    assert_ne!(admin.cookie(), user.cookie());
    assert_ne!(admin.csrf_token(), user.csrf_token());

    let shared_url = server.url().join("created-by-user.txt")?;
    let created_body = "created by user";
    let created = with_new_upload_headers(
        server.request_with(&user, Method::PUT, shared_url.clone()),
        created_body.len() as u64,
    )
    .body(created_body)
    .send()?;
    assert_eq!(created.status(), StatusCode::CREATED);

    let read_by_admin = server.get_with(&admin, shared_url.clone())?;
    assert_eq!(read_by_admin.status(), StatusCode::OK);
    assert_eq!(read_by_admin.text()?, "created by user");

    let overwritten_body = "overwritten by admin";
    let target = preflight_upload_target_with(&server, &admin, "/created-by-user.txt")?;
    assert!(target.exists && target.replaceable);
    let revision = target.revision.ok_or("shared file has no revision")?;
    let overwritten = with_new_upload_overwrite_headers(
        server.request_with(&admin, Method::PUT, shared_url.clone()),
        overwritten_body.len() as u64,
        &revision,
    )
    .body(overwritten_body)
    .send()?;
    assert_eq!(overwritten.status(), StatusCode::CREATED);
    let read_by_user = server.get_with(&user, shared_url.clone())?;
    assert_eq!(read_by_user.text()?, "overwritten by admin");
    let delete_revision = preflight_upload_target_with(&server, &user, "/created-by-user.txt")?
        .revision
        .ok_or("shared delete target has no revision")?;

    let deleted = server
        .request_with(&user, Method::DELETE, shared_url)
        .header("If-Match", format!("\"{delete_revision}\""))
        .send()?;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(!server.path().join("created-by-user.txt").exists());
    Ok(())
}

#[rstest]
fn every_write_method_rejects_missing_forged_and_cross_session_csrf(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let session = server.login("user", TEST_PASSWORD)?;
    let other_session = server.login("user", TEST_PASSWORD)?;
    let csrf_cases = [
        ("missing", None),
        ("forged", Some("A".repeat(43))),
        (
            "cross-session",
            Some(other_session.csrf_token().to_string()),
        ),
    ];

    for (label, csrf) in csrf_cases {
        let csrf = csrf.as_deref();
        let put_name = format!("csrf-{label}-put.txt");
        let put = with_new_upload_headers(
            request_with_csrf(
                &server,
                &session,
                Method::PUT,
                server.url().join(&put_name)?,
                csrf,
            ),
            "blocked".len() as u64,
        )
        .body("blocked")
        .send()?;
        assert_eq!(put.status(), StatusCode::FORBIDDEN, "PUT {label}");
        assert!(!server.path().join(&put_name).exists());

        let patch_name = format!("csrf-{label}-patch.txt");
        let patch_path = server.path().join(&patch_name);
        std::fs::write(&patch_path, "unchanged")?;
        let patch = with_resume_upload_headers(
            request_with_csrf(
                &server,
                &session,
                Method::PATCH,
                server.url().join(&patch_name)?,
                csrf,
            ),
            Uuid::new_v4(),
            "unchanged blocked".len() as u64,
            "unchanged".len() as u64,
        )
        .body(" blocked")
        .send()?;
        assert_eq!(patch.status(), StatusCode::FORBIDDEN, "PATCH {label}");
        assert_eq!(std::fs::read_to_string(patch_path)?, "unchanged");

        let delete_name = format!("csrf-{label}-delete.txt");
        let delete_path = server.path().join(&delete_name);
        std::fs::write(&delete_path, "must remain")?;
        let delete = request_with_csrf(
            &server,
            &session,
            Method::DELETE,
            server.url().join(&delete_name)?,
            csrf,
        )
        .send()?;
        assert_eq!(delete.status(), StatusCode::FORBIDDEN, "DELETE {label}");
        assert_eq!(std::fs::read_to_string(delete_path)?, "must remain");

        let directory_name = format!("csrf-{label}-directory");
        let post = request_with_csrf(
            &server,
            &session,
            Method::POST,
            server.url().join("__dufs__/api/mkdir")?,
            csrf,
        )
        .header(CONTENT_TYPE, "application/json")
        .body(json!({"path": format!("/{directory_name}")}).to_string())
        .send()?;
        assert_eq!(post.status(), StatusCode::FORBIDDEN, "POST {label}");
        assert!(!server.path().join(directory_name).exists());
    }
    Ok(())
}

#[rstest]
fn valid_csrf_allows_put_patch_delete_and_browser_post(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let session = server.login("user", TEST_PASSWORD)?;

    let upload_url = server.url().join("csrf-valid-upload.txt")?;
    let upload_id = Uuid::new_v4();
    let put = with_upload_headers(
        server.request_with(&session, Method::PUT, upload_url.clone()),
        upload_id,
        "first-second".len() as u64,
    )
    .body("first")
    .send()?;
    assert_eq!(put.status(), StatusCode::CONFLICT);
    assert!(!server.path().join("csrf-valid-upload.txt").exists());

    let patch = with_resume_upload_headers(
        server.request_with(&session, Method::PATCH, upload_url.clone()),
        upload_id,
        "first-second".len() as u64,
        "first".len() as u64,
    )
    .body("-second")
    .send()?;
    assert_eq!(patch.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        std::fs::read_to_string(server.path().join("csrf-valid-upload.txt"))?,
        "first-second"
    );

    let mkdir = server
        .request_with(
            &session,
            Method::POST,
            server.url().join("__dufs__/api/mkdir")?,
        )
        .header(CONTENT_TYPE, "application/json")
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .body(json!({"path": "/csrf-valid-directory"}).to_string())
        .send()?;
    assert_eq!(mkdir.status(), StatusCode::CREATED);
    assert!(server.path().join("csrf-valid-directory").is_dir());
    let delete_revision =
        preflight_upload_target_with(&server, &session, "/csrf-valid-upload.txt")?
            .revision
            .ok_or("valid csrf delete target has no revision")?;

    let delete = server
        .request_with(&session, Method::DELETE, upload_url)
        .header("If-Match", format!("\"{delete_revision}\""))
        .send()?;
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    assert!(!server.path().join("csrf-valid-upload.txt").exists());
    Ok(())
}

#[rstest]
fn logout_clears_cookie_and_immediately_revokes_the_session(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let session = server.login("user", TEST_PASSWORD)?;
    let removed_logout = server
        .request_with(
            &session,
            Method::POST,
            server.url().join("__dufs__/logout")?,
        )
        .send()?;
    // Retired auth routes are ordinary unknown paths. Keeping a method-aware
    // response would itself preserve a compatibility alias.
    assert_eq!(removed_logout.status(), StatusCode::NOT_FOUND);
    let still_authenticated = server
        .raw_request(Method::GET, server.url().join("api/v2/auth/session")?)
        .header(COOKIE, session.cookie())
        .send()?;
    assert_eq!(still_authenticated.status(), StatusCode::OK);

    let logout = server
        .request_with(
            &session,
            Method::POST,
            server.url().join("api/v2/auth/logout")?,
        )
        .send()?;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let clear_cookie = logout
        .headers()
        .get(SET_COOKIE)
        .ok_or("Logout did not clear the session cookie")?
        .to_str()?;
    let clear_cookie_lower = clear_cookie.to_ascii_lowercase();
    assert!(clear_cookie.starts_with(&format!("{SESSION_COOKIE_NAME}=;")));
    for attribute in [
        "path=/",
        "httponly",
        "secure",
        "samesite=strict",
        "max-age=0",
    ] {
        assert!(
            clear_cookie_lower.contains(attribute),
            "cookie={clear_cookie}"
        );
    }
    assert!(!clear_cookie_lower.contains("domain="));

    let old_session = server
        .raw_request(Method::GET, server.url())
        .header(COOKIE, session.cookie())
        .send()?;
    assert_eq!(old_session.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[rstest]
fn authorization_header_does_not_replace_browser_session_authentication(
    #[with(&[] as &[&str], &[USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let response = server
        .raw_request(Method::GET, server.url())
        .header(AUTHORIZATION, "Unsupported forged-credentials")
        .send()?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!response.headers().contains_key("www-authenticate"));
    Ok(())
}
