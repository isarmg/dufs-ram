mod fixtures;
mod utils;

use fixtures::{Error, TEST_PASSWORD, TestServer, USER_ACCOUNT, server};
use rstest::rstest;

const HEALTH_CHECK_PATH: &str = "__dufs__/health";
const HEALTH_CHECK_RESPONSE: &str = r#"{"status":"OK"}"#;

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
    assert_eq!(resp.status(), 401);

    let session = server.login("user", TEST_PASSWORD)?;
    let resp = server.get_with(&session, &url)?;
    assert_eq!(resp.text()?, HEALTH_CHECK_RESPONSE);
    Ok(())
}

#[rstest]
fn path_prefix_health(#[with(&["--path-prefix", "xyz"])] server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}xyz/{HEALTH_CHECK_PATH}", server.url()))?;
    assert_eq!(resp.text()?, HEALTH_CHECK_RESPONSE);
    Ok(())
}
