mod fixtures;
mod utils;

use fixtures::{Error, TestServer, server};
use reqwest::header::{CONTENT_RANGE, ETAG, HeaderValue, IF_RANGE, LAST_MODIFIED};
use rstest::rstest;

#[rstest]
fn get_file_range(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header("range", HeaderValue::from_static("bytes=0-6"))
        .send()?;
    assert_eq!(resp.status(), 206);
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes 0-6/18");
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(resp.headers().get("content-length").unwrap(), "7");
    assert_eq!(resp.text()?, "This is");
    Ok(())
}

#[rstest]
fn get_file_range_beyond(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header("range", HeaderValue::from_static("bytes=12-20"))
        .send()?;
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes 12-17/18"
    );
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(resp.headers().get("content-length").unwrap(), "6");
    assert_eq!(resp.text()?, "x.html");
    Ok(())
}

#[rstest]
fn get_file_suffix_larger_than_representation_returns_whole_file(
    server: TestServer,
) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header("range", HeaderValue::from_static("bytes=-999"))
        .send()?;
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes 0-17/18"
    );
    assert_eq!(resp.headers().get("content-length").unwrap(), "18");
    assert_eq!(resp.text()?, "This is index.html");
    Ok(())
}

#[rstest]
fn get_file_range_invalid(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header("range", HeaderValue::from_static("bytes=20-"))
        .send()?;
    assert_eq!(resp.status(), 416);
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */18");
    Ok(())
}

#[rstest]
fn get_file_multiple_ranges_is_rejected(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header("range", HeaderValue::from_static("bytes=0-11, 6-17"))
        .send()?;
    assert_eq!(resp.status(), 416);
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */18");
    assert_eq!(resp.headers().get("content-length").unwrap(), "0");
    Ok(())
}

#[rstest]
fn get_file_multiple_ranges_with_invalid_member_is_rejected(
    server: TestServer,
) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header("range", HeaderValue::from_static("bytes=0-6, 20-30"))
        .send()?;
    assert_eq!(resp.status(), 416);
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */18");
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(resp.headers().get("content-length").unwrap(), "0");
    Ok(())
}

#[rstest]
fn get_file_range_reversed(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header("range", HeaderValue::from_static("bytes=10-1"))
        .send()?;
    assert_eq!(resp.status(), 416);
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */18");
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    Ok(())
}

#[rstest]
fn get_file_multiple_reversed_ranges_are_rejected(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::GET, format!("{}index.html", server.url()))
        .header("range", HeaderValue::from_static("bytes=10-1,20-2"))
        .send()?;
    assert_eq!(resp.status(), 416);
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */18");
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    Ok(())
}

#[rstest]
fn weak_validators_cannot_authorize_if_range(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let head = server.request(reqwest::Method::HEAD, &url).send()?;
    let weak_etag = head
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing ETag")?
        .to_owned();
    assert!(weak_etag.starts_with("W/\""));
    let last_modified = head
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing Last-Modified")?
        .to_owned();

    for if_range in [
        weak_etag.clone(),
        weak_etag
            .strip_prefix("W/")
            .expect("weak ETag prefix")
            .to_owned(),
        last_modified,
    ] {
        let response = server
            .request(reqwest::Method::GET, &url)
            .header("range", HeaderValue::from_static("bytes=0-6"))
            .header(IF_RANGE, if_range)
            .send()?;
        assert_eq!(response.status(), 200);
        assert!(!response.headers().contains_key(CONTENT_RANGE));
        assert_eq!(response.headers().get("content-length").unwrap(), "18");
        assert_eq!(response.text()?, "This is index.html");
    }
    Ok(())
}
