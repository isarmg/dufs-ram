//! Verifies that an authenticated account can use every directory-manager capability.

mod fixtures;
mod utils;

use fixtures::{Error, TestServer, server, with_new_upload_headers};
use rstest::rstest;

#[rstest]
fn account_can_upload_and_overwrite(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let resp = with_new_upload_headers(
        server.request(reqwest::Method::PUT, &url),
        "updated".len() as u64,
    )
    .body(b"updated".to_vec())
    .send()?;
    assert_eq!(resp.status(), 201);
    assert_eq!(server.get(url)?.text()?, "updated");
    Ok(())
}

#[rstest]
fn account_can_delete(server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.html", server.url());
    let resp = server.request(reqwest::Method::DELETE, &url).send()?;
    assert_eq!(resp.status(), 204);
    assert_eq!(server.get(url)?.status(), 404);
    Ok(())
}

#[rstest]
fn account_can_open_missing_directory_before_upload(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}new/path/", server.url()))?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn account_can_search(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q=test.html", server.url()))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(!paths.is_empty());
    assert!(paths.iter().all(|path| path.contains("test.html")));
    Ok(())
}

#[rstest]
fn account_can_download_archive(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?zip", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/zip"
    );
    assert!(resp.headers().contains_key("content-disposition"));
    Ok(())
}
