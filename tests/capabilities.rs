//! Verifies that an authenticated account can use every directory-manager capability.

#[path = "support/fixtures.rs"]
mod fixtures;

use fixtures::{
    Error, TestServer, preflight_upload_target, server, with_new_upload_overwrite_headers,
};
use rstest::rstest;

#[rstest]
fn account_can_upload_and_overwrite(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let target = preflight_upload_target(&server, "/index.html")?;
    assert!(target.exists && target.replaceable);
    let revision = target.revision.ok_or("existing file has no revision")?;
    let resp = with_new_upload_overwrite_headers(
        server.request(reqwest::Method::PUT, &url),
        "updated".len() as u64,
        &revision,
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
