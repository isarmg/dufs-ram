#[path = "support/fixtures.rs"]
mod fixtures;

use fixtures::{Error, TEST_PASSWORD, TestServer, USER_ACCOUNT, server};
use rstest::rstest;

const HEALTH_CHECK_PATH: &str = "__dufs__/health";
const HEALTH_CHECK_RESPONSE: &str = r#"{"status":"OK"}"#;
const READINESS_CHECK_PATH: &str = "__dufs__/ready";
const READINESS_CHECK_RESPONSE: &str = r#"{"status":"ready"}"#;

#[rstest]
fn normal_health(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}{HEALTH_CHECK_PATH}", server.url()))?;
    assert_eq!(resp.text()?, HEALTH_CHECK_RESPONSE);
    Ok(())
}

#[rstest]
fn auth_health(#[with(&["--auth", USER_ACCOUNT])] server: TestServer) -> Result<(), Error> {
    let url = format!("{}{HEALTH_CHECK_PATH}", server.url());
    let resp = server.raw_request(reqwest::Method::GET, &url).send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, HEALTH_CHECK_RESPONSE);

    let ready_url = format!("{}{READINESS_CHECK_PATH}", server.url());
    let unauthenticated = server
        .raw_request(reqwest::Method::GET, &ready_url)
        .send()?;
    assert_eq!(unauthenticated.status(), 401);

    let session = server.login("user", TEST_PASSWORD)?;
    let resp = server.get_with(&session, &ready_url)?;
    assert_eq!(resp.text()?, READINESS_CHECK_RESPONSE);
    Ok(())
}

#[rstest]
fn health_supports_head_and_rejects_other_methods_with_allow(
    server: TestServer,
) -> Result<(), Error> {
    let url = format!("{}{HEALTH_CHECK_PATH}", server.url());
    let head = server.raw_request(reqwest::Method::HEAD, &url).send()?;
    assert_eq!(head.status(), 200);
    assert_eq!(head.text()?, "");

    let rejected = server.raw_request(reqwest::Method::POST, &url).send()?;
    assert_eq!(rejected.status(), 405);
    assert_eq!(rejected.headers().get("allow").unwrap(), "GET, HEAD");
    Ok(())
}

#[rstest]
fn readiness_reports_insufficient_protected_disk_space(
    #[with(&["--min-free-space", "18446744073709551615"])] server: TestServer,
) -> Result<(), Error> {
    let health = server.get(format!("{}{HEALTH_CHECK_PATH}", server.url()))?;
    assert_eq!(health.status(), 200);

    let readiness = server.get(format!("{}{READINESS_CHECK_PATH}", server.url()))?;
    assert_eq!(readiness.status(), 503);
    assert_eq!(readiness.text()?, r#"{"status":"not_ready"}"#);
    Ok(())
}
