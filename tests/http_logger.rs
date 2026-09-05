#[path = "support/fixtures.rs"]
mod fixtures;
use fixtures::{
    Error, TEST_ACCOUNT, TEST_PASSWORD, TEST_USER, USER_ACCOUNT, dufs_command, read_bound_url,
    tmpdir,
};

use assert_cmd::prelude::*;
use assert_fs::fixture::TempDir;
use reqwest::blocking::Client;
use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, SET_COOKIE};
use rstest::rstest;
use sarmg_contracts::AdministratorSession;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread::sleep;
use std::time::{Duration, Instant};
use uuid::Uuid;

const CSRF_HEADER: &str = "x-csrf-token";
static HTTP_LOGGER_TEST_LOCK: Mutex<()> = Mutex::new(());

struct Session {
    cookie: String,
    csrf_token: String,
}

#[rstest]
fn verified_session_user_is_written_to_access_log(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) = spawn_logged_server_with_accounts(
        &tmpdir,
        &["--log-format", "ACCESS $status $remote_user"],
        &[USER_ACCOUNT],
    )?;
    let session = login(port, "user", TEST_PASSWORD)?;

    let response = Client::new()
        .get(format!("http://localhost:{port}"))
        .header(COOKIE, &session.cookie)
        .send()?;
    assert_eq!(response.status(), 200);

    let output = stop_and_read(&mut child)?;
    assert!(output.lines().any(|line| line == "ACCESS 200 user"));
    Ok(())
}

#[rstest]
fn empty_log_format_disables_access_log(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) = spawn_logged_server(&tmpdir, &["--log-format", ""])?;
    let session = login(port, TEST_USER, TEST_PASSWORD)?;
    let response = Client::new()
        .get(format!("http://localhost:{port}"))
        .header(COOKIE, &session.cookie)
        .send()?;
    assert_eq!(response.status(), 200);

    let output = stop_and_read(&mut child)?;
    assert!(!output.contains("test-admin"));
    assert!(!output.contains("POST /api/v2/auth/login"));
    Ok(())
}

#[rstest]
fn invalid_session_is_not_written_to_access_log(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) =
        spawn_logged_server(&tmpdir, &["--log-format", "ACCESS $status $remote_user"])?;
    let response = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(format!("http://localhost:{port}"))
        .header(COOKIE, "sarmg-dufs-ram-session=forged-admin")
        .send()?;
    assert_eq!(response.status(), 401);

    let output = stop_and_read(&mut child)?;
    assert!(output.lines().any(|line| line == "ACCESS 401 -"));
    assert!(!output.contains("forged-admin"));
    Ok(())
}

#[rstest]
fn authenticated_logout_keeps_verified_user_in_access_log(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) =
        spawn_logged_server(&tmpdir, &["--log-format", "ACCESS $status $remote_user"])?;
    let session = login(port, TEST_USER, TEST_PASSWORD)?;
    let response = Client::new()
        .post(format!("http://localhost:{port}/api/v2/auth/logout"))
        .header(COOKIE, &session.cookie)
        .header("origin", format!("http://localhost:{port}"))
        .header("sec-fetch-site", "same-origin")
        .header(CSRF_HEADER, &session.csrf_token)
        .send()?;
    assert_eq!(response.status(), 204);

    let output = stop_and_read(&mut child)?;
    assert!(output.lines().any(|line| line == "ACCESS 204 test-admin"));
    Ok(())
}

#[rstest]
fn sensitive_request_headers_are_redacted(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) = spawn_logged_server_with_accounts(
        &tmpdir,
        &[
            "--log-format",
            "SECRETS $http_cookie $http_x_csrf_token $http_authorization \
             $http_proxy_authorization",
        ],
        &[USER_ACCOUNT],
    )?;
    let session = login(port, "user", TEST_PASSWORD)?;
    let response = Client::new()
        .get(format!("http://localhost:{port}"))
        .header(COOKIE, &session.cookie)
        .header(CSRF_HEADER, &session.csrf_token)
        .header("authorization", "sensitive-secret")
        .header("proxy-authorization", "proxy-sensitive-secret")
        .send()?;
    assert_eq!(response.status(), 200);

    let output = stop_and_read(&mut child)?;
    assert!(
        output
            .lines()
            .any(|line| line == "SECRETS [REDACTED] [REDACTED] [REDACTED] [REDACTED]")
    );
    assert!(!output.contains(&session.cookie));
    assert!(!output.contains(&session.csrf_token));
    assert!(!output.contains("sensitive-secret"));
    assert!(!output.contains("proxy-sensitive-secret"));
    Ok(())
}

#[rstest]
fn sensitive_request_header_variable_suffixes_are_case_insensitive(
    tmpdir: TempDir,
) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) = spawn_logged_server_with_accounts(
        &tmpdir,
        &[
            "--log-format",
            "CASE_SECRETS $http_AUTHORIZATION $http_AuThOrIzAtIoN \
             $http_PROXY_AUTHORIZATION $http_PrOxY_AuThOrIzAtIoN \
             $http_COOKIE $http_CoOkIe \
             $http_X_CSRF_TOKEN $http_X_CsRf_ToKeN",
        ],
        &[USER_ACCOUNT],
    )?;
    let session = login(port, "user", TEST_PASSWORD)?;
    let authorization_secret = "case-authorization-secret";
    let proxy_authorization_secret = "case-proxy-authorization-secret";
    let cookie_secret = "case-cookie-secret";
    let csrf_secret = "case-csrf-secret";
    let request_cookie = format!("{}; case-cookie={cookie_secret}", session.cookie);
    let response = Client::new()
        .get(format!("http://localhost:{port}"))
        .header(COOKIE, request_cookie)
        .header(CSRF_HEADER, csrf_secret)
        .header("authorization", authorization_secret)
        .header("proxy-authorization", proxy_authorization_secret)
        .send()?;
    assert_eq!(response.status(), 200);

    let output = stop_and_read(&mut child)?;
    assert!(output.lines().any(|line| {
        line == "CASE_SECRETS [REDACTED] [REDACTED] [REDACTED] [REDACTED] \
                 [REDACTED] [REDACTED] [REDACTED] [REDACTED]"
    }));
    for secret in [
        authorization_secret,
        proxy_authorization_secret,
        cookie_secret,
        csrf_secret,
    ] {
        assert!(!output.contains(secret));
    }
    assert!(!output.contains(&session.cookie));
    Ok(())
}

#[rstest]
fn mixed_case_non_sensitive_request_header_variable_is_logged(
    tmpdir: TempDir,
) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) = spawn_logged_server_with_accounts(
        &tmpdir,
        &["--log-format", "REQUEST_ID $http_X_ReQuEsT_Id"],
        &[USER_ACCOUNT],
    )?;
    let session = login(port, "user", TEST_PASSWORD)?;
    let response = Client::new()
        .get(format!("http://localhost:{port}"))
        .header(COOKIE, &session.cookie)
        .header("x-request-id", "desktop-request-123")
        .send()?;
    assert_eq!(response.status(), 200);

    let output = stop_and_read(&mut child)?;
    assert!(
        output
            .lines()
            .any(|line| line == "REQUEST_ID desktop-request-123")
    );
    Ok(())
}

#[rstest]
fn complete_request_line_keeps_raw_target_and_http_version(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) =
        spawn_logged_server(&tmpdir, &["--log-format", "REQUEST $request"])?;
    let session = login(port, TEST_USER, TEST_PASSWORD)?;

    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(
        format!(
            "GET /a%2Fb?value=%2F HTTP/1.0\r\nHost: localhost:{port}\r\nCookie: {}\r\nConnection: close\r\n\r\n",
            session.cookie
        )
        .as_bytes(),
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    assert!(response.starts_with("HTTP/1.0 "));

    let output = stop_and_read(&mut child)?;
    assert!(
        output
            .lines()
            .any(|line| line == "REQUEST GET /a%2Fb?value=%2F HTTP/1.0"),
        "raw HTTP/1.0 request line was not preserved:\n{output}"
    );
    Ok(())
}

#[rstest]
fn truncated_download_is_logged_as_a_stream_failure(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let download_path = tmpdir.path().join("truncated.bin");
    let download = std::fs::File::create(&download_path)?;
    download.set_len(64 * 1024 * 1024)?;
    drop(download);

    let (mut child, port, _state_dir) = spawn_logged_server(
        &tmpdir,
        &["--log-format", "STREAM $request $log_level $status"],
    )?;
    let session = login(port, TEST_USER, TEST_PASSWORD)?;
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(
        format!(
            "GET /truncated.bin HTTP/1.1\r\nHost: localhost:{port}\r\nCookie: {}\r\nConnection: close\r\n\r\n",
            session.cookie
        )
        .as_bytes(),
    )?;

    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        headers.push(byte[0]);
        if headers.len() > 64 * 1024 {
            return Err("Download response headers exceeded their limit".into());
        }
    }
    let headers = String::from_utf8(headers)?;
    assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
    assert!(
        headers.contains("content-length: 67108864\r\n"),
        "{headers}"
    );

    std::fs::OpenOptions::new()
        .write(true)
        .open(&download_path)?
        .set_len(0)?;
    let mut remainder = Vec::new();
    let _ = stream.read_to_end(&mut remainder);

    let output = stop_and_read(&mut child)?;
    assert!(
        output.lines().any(|line| {
            line.starts_with("STREAM GET /truncated.bin HTTP/1.1 ERROR 200 ")
                && line.contains("response body stream failed:")
                && line.contains("ended before its advertised length")
        }),
        "truncated download was not logged as a response stream failure:\n{output}"
    );
    Ok(())
}

#[rstest]
fn authenticated_operation_id_is_available_to_access_logs(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let (mut child, port, _state_dir) = spawn_logged_server(
        &tmpdir,
        &[
            "--log-format",
            "OPERATION $operation_id $operation_state $status",
        ],
    )?;
    let session = login(port, TEST_USER, TEST_PASSWORD)?;
    let operation_id = Uuid::new_v4().to_string();
    let response = Client::new()
        .post(format!("http://localhost:{port}/__dufs__/api/mkdir"))
        .header(COOKIE, &session.cookie)
        .header("origin", format!("http://localhost:{port}"))
        .header("sec-fetch-site", "same-origin")
        .header(CSRF_HEADER, &session.csrf_token)
        .header(CONTENT_TYPE, "application/json")
        .header("X-Dufs-Operation-Id", &operation_id)
        .body(r#"{"path":"/logged-operation"}"#)
        .send()?;
    assert_eq!(response.status(), 201);
    assert_eq!(
        response
            .headers()
            .get("x-dufs-operation-id")
            .and_then(|value| value.to_str().ok()),
        Some(operation_id.as_str())
    );

    let output = stop_and_read(&mut child)?;
    let expected = format!("OPERATION {operation_id} succeeded 201");
    assert!(
        output.lines().any(|line| line == expected),
        "missing operation ID log line: {expected}\n{output}"
    );
    Ok(())
}

#[rstest]
fn only_successful_embedded_asset_gets_are_omitted_from_access_log(
    tmpdir: TempDir,
) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let args = ["--log-format", "P006 $request_method $request_uri $status"];
    let (mut child, port, _state_dir) = spawn_logged_server(&tmpdir, &args)?;
    let session = login(port, TEST_USER, TEST_PASSWORD)?;
    let client = Client::new();
    let page_path = "/";
    let page = client
        .get(format!("http://localhost:{port}{page_path}"))
        .header(COOKIE, &session.cookie)
        .send()?
        .error_for_status()?
        .text()?;

    let mut asset_paths = Vec::new();
    for filename in ["index.js", "index.css", "favicon.ico"] {
        let asset_path = extract_embedded_asset_path(&page, filename)?;
        let response = client
            .get(format!("http://localhost:{port}{asset_path}"))
            .header(COOKIE, &session.cookie)
            .send()?;
        assert_eq!(response.status(), 200, "asset={asset_path}");
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("public, max-age=31536000, immutable"),
            "asset={asset_path}"
        );
        asset_paths.push(asset_path);
    }

    let download_path = "/test.txt";
    let response = client
        .get(format!("http://localhost:{port}{download_path}"))
        .header(COOKIE, &session.cookie)
        .send()?;
    assert_eq!(response.status(), 200);

    let api_path = "/__dufs__/api/mkdir";
    let operation_id = Uuid::new_v4().to_string();
    let response = client
        .post(format!("http://localhost:{port}{api_path}"))
        .header(COOKIE, &session.cookie)
        .header("origin", format!("http://localhost:{port}"))
        .header("sec-fetch-site", "same-origin")
        .header(CSRF_HEADER, &session.csrf_token)
        .header(CONTENT_TYPE, "application/json")
        .header("X-Dufs-Operation-Id", operation_id)
        .body(r#"{"path":"/logged-api-directory"}"#)
        .send()?;
    assert_eq!(response.status(), 201);

    let index_js_path = &asset_paths[0];
    let anonymous_asset_path = format!("{index_js_path}?anonymous=1");
    let response = client
        .get(format!("http://localhost:{port}{anonymous_asset_path}"))
        .send()?;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );

    let asset_directory = index_js_path
        .strip_suffix("index.js")
        .ok_or("Embedded index.js path has an unexpected form")?;
    let missing_path = format!("{asset_directory}missing.js");
    let response = client
        .get(format!("http://localhost:{port}{missing_path}"))
        .send()?;
    assert_eq!(response.status(), 404);
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("private, no-store")
    );

    let head_asset_path = format!("{index_js_path}?head=1");
    let response = client
        .head(format!("http://localhost:{port}{head_asset_path}"))
        .send()?;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    assert!(
        response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.parse::<usize>().is_ok_and(|value| value > 0))
    );
    assert_eq!(response.text()?, "");

    let output = stop_and_read(&mut child)?;
    for asset_path in &asset_paths {
        let unexpected = format!("P006 GET {asset_path} 200");
        assert!(
            !output.lines().any(|line| line == unexpected),
            "successful embedded asset was logged: {unexpected}\n{output}"
        );
    }
    let unexpected = format!("P006 GET {anonymous_asset_path} 200");
    assert!(
        !output.lines().any(|line| line == unexpected),
        "anonymous successful embedded asset was logged: {unexpected}\n{output}"
    );

    let login_path = "/api/v2/auth/login";
    for expected in [
        format!("P006 POST {login_path} 200"),
        format!("P006 GET {page_path} 200"),
        format!("P006 GET {download_path} 200"),
        format!("P006 POST {api_path} 201"),
        format!("P006 GET {missing_path} 404"),
        format!("P006 HEAD {head_asset_path} 200"),
    ] {
        assert!(
            output.lines().any(|line| line == expected),
            "missing access log line: {expected}\n{output}"
        );
    }
    Ok(())
}

#[rstest]
fn malformed_http_connection_logs_peer_and_category(tmpdir: TempDir) -> Result<(), Error> {
    let _test_guard = serialize_http_logger_test();
    let log_dir = TempDir::new()?;
    let log_file = log_dir.path().join("connection.log");
    let state_dir = private_state_dir()?;
    let (mut command, _auth_config) = dufs_command(&[TEST_ACCOUNT]);
    let mut child = command
        .arg(tmpdir.path())
        .arg("-p")
        .arg("0")
        .args(["--bind", "127.0.0.1"])
        .arg("--state-dir")
        .arg(state_dir.path())
        .arg("--log-file")
        .arg(&log_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let port = read_bound_url(&mut child)?
        .port()
        .ok_or("Printed URL has no port")?;
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.write_all(b"GET / HTTP/1.1\r\nInvalid Header\r\n\r\n")?;
    if let Err(err) = stream.shutdown(Shutdown::Write)
        && err.kind() != std::io::ErrorKind::NotConnected
    {
        return Err(err.into());
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    let output = loop {
        let output = std::fs::read_to_string(&log_file).unwrap_or_default();
        if output.contains("HTTP connection error peer=127.0.0.1:") {
            break output;
        }
        if Instant::now() >= deadline {
            break output;
        }
        sleep(Duration::from_millis(20));
    };

    child.kill()?;
    child.wait()?;
    assert!(output.contains(" WARN HTTP connection error peer=127.0.0.1:"));
    assert!(output.contains("category=protocol"));
    assert!(output.contains("request_seen=false"));
    assert!(output.contains("io_kind=-"));
    assert_eq!(output.matches("HTTP connection error").count(), 1);
    Ok(())
}

#[rstest]
fn startup_failure_is_written_and_flushed_to_the_configured_log(
    tmpdir: TempDir,
) -> Result<(), Error> {
    let state_dir = private_state_dir()?;
    let log_dir = TempDir::new()?;
    let log_file = log_dir.path().join("startup.log");
    let (mut command, _auth_config) = dufs_command(&[TEST_ACCOUNT]);
    command
        .arg(tmpdir.path())
        .arg("--state-dir")
        .arg(state_dir.path())
        .args(["--bind", "20.205.243.166", "--port", "0"])
        .arg("--log-file")
        .arg(&log_file)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Failed to bind"));

    let output = std::fs::read_to_string(&log_file)?;
    assert!(
        output.contains("ERROR Server failed: Failed to bind `20.205.243.166:0`"),
        "startup failure was not flushed to the configured log: {output:?}"
    );
    Ok(())
}

fn serialize_http_logger_test() -> MutexGuard<'static, ()> {
    HTTP_LOGGER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_logged_server(tmpdir: &TempDir, args: &[&str]) -> Result<(Child, u16, TempDir), Error> {
    spawn_logged_server_with_accounts(tmpdir, args, &[TEST_ACCOUNT])
}

fn spawn_logged_server_with_accounts(
    tmpdir: &TempDir,
    args: &[&str],
    accounts: &[&str],
) -> Result<(Child, u16, TempDir), Error> {
    let state_dir = private_state_dir()?;
    let (mut command, _auth_config) = dufs_command(accounts);
    let mut child = command
        .arg("--development")
        .arg(tmpdir.path())
        .arg("-p")
        .arg("0")
        .args(args)
        .arg("--state-dir")
        .arg(state_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let port = read_bound_url(&mut child)?
        .port()
        .ok_or("Printed URL has no port")?;
    Ok((child, port, state_dir))
}

fn private_state_dir() -> Result<TempDir, Error> {
    let state_dir = TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(state_dir)
}

fn login(port: u16, username: &str, password: &str) -> Result<Session, Error> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .post(format!("http://localhost:{port}/api/v2/auth/login"))
        .header("origin", format!("http://localhost:{port}"))
        .header("sec-fetch-site", "same-origin")
        .header(CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&serde_json::json!({
            "username": username,
            "password": password,
        }))?)
        .send()?;
    assert_eq!(response.status(), 200);
    let cookie = response
        .headers()
        .get(SET_COOKIE)
        .ok_or("Missing session cookie")?
        .to_str()?
        .split(';')
        .next()
        .ok_or("Invalid session cookie")?
        .to_string();
    let session: AdministratorSession = serde_json::from_str(&response.text()?)?;
    session.validate()?;
    let csrf_token = session.csrf_token;
    Ok(Session { cookie, csrf_token })
}

fn extract_embedded_asset_path(page: &str, filename: &str) -> Result<String, Error> {
    let marker = format!("{filename}\"");
    let filename_start = page
        .find(&marker)
        .ok_or("Rendered page is missing an embedded asset")?;
    let value_start = page[..filename_start]
        .rfind('"')
        .ok_or("Embedded asset URL is missing its opening quote")?
        + 1;
    let value_end = filename_start + filename.len();
    let path = &page[value_start..value_end];
    if !path.starts_with('/') {
        return Err("Embedded asset URL is not an absolute path".into());
    }
    Ok(path.to_string())
}

fn stop_and_read(child: &mut Child) -> Result<String, Error> {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()?;
    if !status.success() {
        return Err("Failed to send SIGTERM to logged test server".into());
    }
    let mut output = String::new();
    child
        .stderr
        .take()
        .ok_or("Missing child stderr")?
        .read_to_string(&mut output)?;
    child.wait()?;

    let mut unexpected_stdout = String::new();
    child
        .stdout
        .take()
        .ok_or("Missing child stdout")?
        .read_to_string(&mut unexpected_stdout)?;
    assert!(
        unexpected_stdout.is_empty(),
        "console log leaked into the startup-address stream: {unexpected_stdout:?}"
    );
    Ok(output)
}
