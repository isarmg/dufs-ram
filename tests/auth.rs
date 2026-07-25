mod fixtures;

use fixtures::{
    ADMIN_ACCOUNT, Error, TEST_PASSWORD, TestServer, TestSession, USER_ACCOUNT, server, tmpdir,
    with_new_upload_headers, with_resume_upload_headers, with_upload_headers,
};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use reqwest::{Method, StatusCode, Url};
use rstest::rstest;
use serde_json::json;
use std::process::Command;
use uuid::Uuid;

const CSRF_HEADER: &str = "x-dufs-csrf-token";
const SESSION_COOKIE_NAME: &str = "__Host-dufs-session";

fn client_without_redirects() -> Result<Client, Error> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn login_form_response(
    server: &TestServer,
    username: &str,
    password: &str,
) -> Result<Response, Error> {
    let form = form_urlencoded::Serializer::new(String::new())
        .append_pair("username", username)
        .append_pair("password", password)
        .finish();
    Ok(client_without_redirects()?
        .post(server.url().join("__dufs__/login")?)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form)
        .send()?)
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
        .header(COOKIE, session.cookie());
    match csrf {
        Some(csrf) => request.header(CSRF_HEADER, csrf),
        None => request,
    }
}

#[rstest]
fn unauthenticated_html_navigation_redirects_to_login(
    #[with(&["--auth", USER_ACCOUNT])] server: TestServer,
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
    let body = login_page.text()?;
    assert!(body.contains("<form"));
    assert!(body.contains("class=\"content-card login-card\""));
    assert!(body.contains("aspect-ratio: 3 / 2"));
    assert!(body.contains("grid-template-rows: repeat(6, minmax(0, 1fr))"));
    assert!(body.contains("novalidate"));
    assert_eq!(body.matches(" required").count(), 2);
    assert!(body.contains("name=\"username\""));
    assert!(body.contains("name=\"password\""));
    Ok(())
}

#[rstest]
fn unauthenticated_writes_return_401_before_touching_disk(
    #[with(&["--auth", USER_ACCOUNT])] server: TestServer,
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
fn login_form_uses_one_time_prg_errors_and_sets_cookie_only_on_success(
    #[with(&["--auth", USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let client = client_without_redirects()?;
    let mut locations = Vec::new();
    for (username, password, message) in [
        ("", "", "请填写账号和密码"),
        ("user", "", "请填写账号和密码"),
        ("", TEST_PASSWORD, "请填写账号和密码"),
        ("user", "wrong", "用户名或密码错误。"),
    ] {
        let rejected = login_form_response(&server, username, password)?;
        assert_eq!(rejected.status(), StatusCode::SEE_OTHER);
        assert!(!rejected.headers().contains_key(SET_COOKIE));
        let location = rejected
            .headers()
            .get(LOCATION)
            .ok_or("Rejected login is missing its redirect")?
            .to_str()?
            .to_string();
        assert!(location.starts_with("/__dufs__/login?login_error="));
        assert_eq!(
            location
                .split_once("login_error=")
                .map(|(_, token)| token.len()),
            Some(64)
        );
        assert!(!locations.contains(&location));
        locations.push(location.clone());

        let error_url = server.url().join(&location)?;
        let first_get = client.get(error_url.clone()).send()?;
        assert_eq!(first_get.status(), StatusCode::OK);
        assert!(first_get.text()?.contains(message));

        let refreshed = client.get(error_url).send()?;
        assert_eq!(refreshed.status(), StatusCode::OK);
        let refreshed_body = refreshed.text()?;
        assert!(!refreshed_body.contains("请填写账号和密码"));
        assert!(!refreshed_body.contains("用户名或密码错误。"));
    }

    let accepted = login_form_response(&server, "user", TEST_PASSWORD)?;
    assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
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
    Ok(())
}

#[rstest]
fn cross_site_login_and_write_requests_are_rejected(
    #[with(&["--auth", USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let form = form_urlencoded::Serializer::new(String::new())
        .append_pair("username", "user")
        .append_pair("password", TEST_PASSWORD)
        .finish();
    let login = client_without_redirects()?
        .post(server.url().join("__dufs__/login")?)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("origin", "https://evil.example")
        .header("sec-fetch-site", "cross-site")
        .body(form)
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
    let server = server(&["--auth", USER_ACCOUNT]);

    let session = server.login("user", TEST_PASSWORD)?;
    assert_eq!(
        server.get_with(&session, server.url())?.status(),
        StatusCode::OK
    );
    Ok(())
}

#[rstest]
fn invalid_password_hash_is_rejected_at_startup(
    tmpdir: assert_fs::fixture::TempDir,
) -> Result<(), Error> {
    let output = Command::new(assert_cmd::cargo::cargo_bin!())
        .arg(tmpdir.path())
        .args(["--auth", "user:not-an-argon2id-phc"])
        .output()?;
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
    #[with(&["--auth", ADMIN_ACCOUNT, "--auth", USER_ACCOUNT])] server: TestServer,
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
    let overwritten = with_new_upload_headers(
        server.request_with(&admin, Method::PUT, shared_url.clone()),
        overwritten_body.len() as u64,
    )
    .body(overwritten_body)
    .send()?;
    assert_eq!(overwritten.status(), StatusCode::CREATED);
    let read_by_user = server.get_with(&user, shared_url.clone())?;
    assert_eq!(read_by_user.text()?, "overwritten by admin");

    let deleted = server
        .request_with(&user, Method::DELETE, shared_url)
        .send()?;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(!server.path().join("created-by-user.txt").exists());
    Ok(())
}

#[rstest]
fn every_write_method_rejects_missing_forged_and_cross_session_csrf(
    #[with(&["--auth", USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let session = server.login("user", TEST_PASSWORD)?;
    let other_session = server.login("user", TEST_PASSWORD)?;
    let csrf_cases = [
        ("missing", None),
        ("forged", Some("00".repeat(32))),
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
    #[with(&["--auth", USER_ACCOUNT])] server: TestServer,
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
        .body(json!({"path": "/csrf-valid-directory"}).to_string())
        .send()?;
    assert_eq!(mkdir.status(), StatusCode::CREATED);
    assert!(server.path().join("csrf-valid-directory").is_dir());

    let delete = server
        .request_with(&session, Method::DELETE, upload_url)
        .send()?;
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    assert!(!server.path().join("csrf-valid-upload.txt").exists());
    Ok(())
}

#[rstest]
fn logout_clears_cookie_and_immediately_revokes_the_session(
    #[with(&["--auth", USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let session = server.login("user", TEST_PASSWORD)?;
    let logout = server
        .request_with(
            &session,
            Method::POST,
            server.url().join("__dufs__/logout")?,
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
    #[with(&["--auth", USER_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let response = server
        .raw_request(Method::GET, server.url())
        .header(AUTHORIZATION, "Unsupported forged-credentials")
        .send()?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!response.headers().contains_key("www-authenticate"));
    Ok(())
}

#[rstest]
fn path_prefix_uses_its_own_login_route_and_session(
    #[with(&["--auth", USER_ACCOUNT, "--path-prefix", "xyz"])] server: TestServer,
) -> Result<(), Error> {
    let client = client_without_redirects()?;
    let prefixed_root = server.url().join("xyz/")?;
    let redirect = client
        .get(prefixed_root.clone())
        .header(ACCEPT, "text/html")
        .send()?;
    assert_eq!(redirect.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        redirect
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/xyz/__dufs__/login")
    );

    let form = form_urlencoded::Serializer::new(String::new())
        .append_pair("username", "")
        .append_pair("password", "")
        .finish();
    let rejected = client
        .post(server.url().join("xyz/__dufs__/login")?)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form)
        .send()?;
    assert_eq!(rejected.status(), StatusCode::SEE_OTHER);
    let location = rejected
        .headers()
        .get(LOCATION)
        .ok_or("Prefixed login failure is missing its redirect")?
        .to_str()?;
    assert!(location.starts_with("/xyz/__dufs__/login?login_error="));

    let session = server.login("user", TEST_PASSWORD)?;
    assert_eq!(
        server.get_with(&session, prefixed_root)?.status(),
        StatusCode::OK
    );
    Ok(())
}
