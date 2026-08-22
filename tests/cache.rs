#[path = "support/fixtures.rs"]
mod fixtures;

use chrono::{DateTime, Duration};
use fixtures::{Error, TestServer, server, with_new_upload_headers};
use reqwest::StatusCode;
use reqwest::header::{
    CACHE_CONTROL, ETAG, HeaderMap, HeaderName, IF_MATCH, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    IF_UNMODIFIED_SINCE, LAST_MODIFIED, RANGE,
};
use rstest::rstest;
use std::{
    fs::{File, FileTimes},
    time::{Duration as StdDuration, SystemTime},
};
use uuid::Uuid;

fn assert_private_no_store(headers: &HeaderMap) -> Result<(), Error> {
    let value = headers
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing Cache-Control")?;
    let mut directives = value
        .split(',')
        .map(str::trim)
        .filter(|directive| !directive.is_empty())
        .collect::<Vec<_>>();
    directives.sort_unstable();
    assert_eq!(directives, ["no-store", "private"]);
    Ok(())
}

#[rstest]
#[case(IF_UNMODIFIED_SINCE, Duration::days(1), StatusCode::OK)]
#[case(IF_UNMODIFIED_SINCE, Duration::days(0), StatusCode::OK)]
#[case(IF_UNMODIFIED_SINCE, Duration::days(-1), StatusCode::PRECONDITION_FAILED)]
#[case(IF_MODIFIED_SINCE, Duration::days(1), StatusCode::NOT_MODIFIED)]
#[case(IF_MODIFIED_SINCE, Duration::days(0), StatusCode::NOT_MODIFIED)]
#[case(IF_MODIFIED_SINCE, Duration::days(-1), StatusCode::OK)]
fn get_file_with_if_modified_since_condition(
    #[case] header_condition: HeaderName,
    #[case] duration_after_file_modified: Duration,
    #[case] expected_code: StatusCode,
    server: TestServer,
) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}index.html", server.url()))
        .send()?;
    assert_private_no_store(resp.headers())?;

    let last_modified = resp
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
        .expect("Received no valid last modified header");

    let req_modified_time = (last_modified + duration_after_file_modified)
        .format("%a, %d %b %Y %T GMT")
        .to_string();

    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header(header_condition, req_modified_time)
        .send()?;

    assert_eq!(resp.status(), expected_code);
    assert_private_no_store(resp.headers())?;
    Ok(())
}

fn same_etag(etag: &str) -> String {
    etag.to_owned()
}

fn different_etag(_: &str) -> String {
    r#"W/"different""#.to_owned()
}

#[rstest]
#[case(IF_MATCH, same_etag, StatusCode::PRECONDITION_FAILED)]
#[case(IF_MATCH, different_etag, StatusCode::PRECONDITION_FAILED)]
#[case(IF_NONE_MATCH, same_etag, StatusCode::NOT_MODIFIED)]
#[case(IF_NONE_MATCH, different_etag, StatusCode::OK)]
fn get_file_with_etag_match(
    #[case] header_condition: HeaderName,
    #[case] etag_modifier: fn(&str) -> String,
    #[case] expected_code: StatusCode,
    server: TestServer,
) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}index.html", server.url()))
        .send()?;
    assert_private_no_store(resp.headers())?;

    let etag = resp
        .headers()
        .get(ETAG)
        .and_then(|h| h.to_str().ok())
        .expect("Received no valid etag header");
    assert!(
        etag.starts_with("W/\"") && etag.ends_with('"'),
        "server must emit a syntactically valid weak ETag: {etag}"
    );

    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header(header_condition, etag_modifier(etag))
        .send()?;

    assert_eq!(resp.status(), expected_code);
    assert_private_no_store(resp.headers())?;
    Ok(())
}

#[rstest]
fn if_none_match_takes_precedence_over_if_modified_since(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let head = server.request(reqwest::Method::HEAD, &url).send()?;
    assert_private_no_store(head.headers())?;
    let etag = head
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing ETag")?
        .to_owned();
    let last_modified = head
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| DateTime::parse_from_rfc2822(value).ok())
        .ok_or("Missing Last-Modified")?;

    let future = (last_modified + Duration::days(1))
        .format("%a, %d %b %Y %T GMT")
        .to_string();
    let response = server
        .request(reqwest::Method::GET, &url)
        .header(IF_NONE_MATCH, r#"W/"different""#)
        .header(IF_MODIFIED_SINCE, future)
        .send()?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_private_no_store(response.headers())?;
    assert_eq!(response.text()?, "This is index.html");

    let past = (last_modified - Duration::days(1))
        .format("%a, %d %b %Y %T GMT")
        .to_string();
    let response = server
        .request(reqwest::Method::GET, &url)
        .header(IF_NONE_MATCH, &etag)
        .header(IF_MODIFIED_SINCE, past)
        .send()?;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_private_no_store(response.headers())?;
    assert_eq!(response.headers().get(ETAG).unwrap(), etag.as_str());
    assert!(response.headers().contains_key(LAST_MODIFIED));
    assert_eq!(response.text()?, "");
    Ok(())
}

#[rstest]
fn if_match_takes_precedence_over_if_unmodified_since(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let head = server.request(reqwest::Method::HEAD, &url).send()?;
    assert_private_no_store(head.headers())?;
    let last_modified = head
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| DateTime::parse_from_rfc2822(value).ok())
        .ok_or("Missing Last-Modified")?;
    let stale = (last_modified - Duration::days(1))
        .format("%a, %d %b %Y %T GMT")
        .to_string();

    let response = server
        .request(reqwest::Method::GET, &url)
        .header(IF_MATCH, "*")
        .header(IF_UNMODIFIED_SINCE, stale)
        .send()?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_private_no_store(response.headers())?;
    assert_eq!(response.text()?, "This is index.html");
    Ok(())
}

#[rstest]
fn same_size_same_mtime_atomic_replacement_changes_weak_etag(
    server: TestServer,
) -> Result<(), Error> {
    let path = server.path().join("same-size.txt");
    let replacement = server.path().join("same-size-replacement.txt");
    let modified = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_700_000_000);
    std::fs::write(&path, b"old!")?;
    std::fs::write(&replacement, b"new!")?;
    File::open(&path)?.set_times(FileTimes::new().set_modified(modified))?;
    File::open(&replacement)?.set_times(FileTimes::new().set_modified(modified))?;

    let url = format!("{}same-size.txt", server.url());
    let old_response = server.request(reqwest::Method::HEAD, &url).send()?;
    assert_private_no_store(old_response.headers())?;
    let old_etag = old_response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing old ETag")?
        .to_owned();
    let old_last_modified = old_response
        .headers()
        .get(LAST_MODIFIED)
        .cloned()
        .ok_or("Missing old Last-Modified")?;

    std::fs::rename(&replacement, &path)?;

    let new_response = server.request(reqwest::Method::HEAD, &url).send()?;
    assert_private_no_store(new_response.headers())?;
    let new_etag = new_response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing new ETag")?
        .to_owned();
    assert!(old_etag.starts_with("W/\""));
    assert!(new_etag.starts_with("W/\""));
    assert_ne!(old_etag, new_etag);
    assert_eq!(
        new_response.headers().get(LAST_MODIFIED),
        Some(&old_last_modified)
    );

    let response = server
        .request(reqwest::Method::GET, &url)
        .header(IF_NONE_MATCH, old_etag)
        .send()?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_private_no_store(response.headers())?;
    assert_eq!(response.text()?, "new!");
    Ok(())
}

#[rstest]
fn range_responses_are_private_and_never_stored(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());

    let partial = server
        .request(reqwest::Method::GET, &url)
        .header(RANGE, "bytes=0-3")
        .send()?;
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_private_no_store(partial.headers())?;
    assert_eq!(partial.text()?, "This");

    let unsatisfiable = server
        .request(reqwest::Method::GET, &url)
        .header(RANGE, "bytes=999999-")
        .send()?;
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_private_no_store(unsatisfiable.headers())?;
    Ok(())
}

#[rstest]
fn non_download_and_error_responses_are_private_and_never_stored(
    server: TestServer,
) -> Result<(), Error> {
    let login = server
        .raw_request(
            reqwest::Method::GET,
            format!("{}__dufs__/login", server.url()),
        )
        .send()?;
    assert_eq!(login.status(), StatusCode::OK);
    assert_private_no_store(login.headers())?;

    let missing = server.get(format!("{}missing.txt", server.url()))?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_private_no_store(missing.headers())?;

    let upload_url = format!("{}cache-policy.txt", server.url());
    let created = with_new_upload_headers(
        server.request(reqwest::Method::PUT, &upload_url),
        "contents".len() as u64,
    )
    .body("contents")
    .send()?;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_private_no_store(created.headers())?;

    let deleted = server
        .request(reqwest::Method::DELETE, &upload_url)
        .send()?;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_private_no_store(deleted.headers())?;

    let api = server
        .request(
            reqwest::Method::POST,
            format!("{}__dufs__/api/mkdir", server.url()),
        )
        .header("content-type", "application/json")
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .body(r#"{"path":"/cache-policy-directory"}"#)
        .send()?;
    assert_eq!(api.status(), StatusCode::CREATED);
    assert_private_no_store(api.headers())?;
    Ok(())
}
