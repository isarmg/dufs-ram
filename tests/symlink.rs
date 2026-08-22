#[path = "support/fixtures.rs"]
mod fixtures;
#[path = "support/utils.rs"]
mod utils;

use assert_fs::fixture::TempDir;
use fixtures::{
    Error, TestServer, preflight_upload_target, server, tmpdir, with_new_upload_headers,
    with_new_upload_overwrite_headers,
};
use reqwest::Method;
use rstest::rstest;
use serde_json::{Value, json};

use std::os::unix::fs::symlink;

fn assert_invalid_preflight_path(server: &TestServer, path: &str) -> Result<(), Error> {
    let response = server
        .request(
            Method::POST,
            server.url().join("__dufs__/api/upload/preflight")?,
        )
        .header("content-type", "application/json")
        .body(json!({ "paths": [path] }).to_string())
        .send()?;
    assert_eq!(response.status(), 400);
    let problem: Value = serde_json::from_str(&response.text()?)?;
    assert_eq!(problem["code"], "invalid_path");
    Ok(())
}

#[rstest]
fn outside_symlink_is_not_served(server: TestServer, tmpdir: TempDir) -> Result<(), Error> {
    // Create symlink directory "foo" to point outside the root
    let dir = "foo";
    symlink(tmpdir.path(), server.path().join(dir)).expect("Couldn't create symlink");
    let resp = server.get(format!("{}{}", server.url(), dir))?;
    assert_eq!(resp.status(), 404);
    let resp = server.get(format!("{}{}/index.html", server.url(), dir))?;
    assert_eq!(resp.status(), 404);
    let resp = server.get(server.url())?;
    let paths = server.paths_from_page(resp)?;
    assert!(!paths.is_empty());
    assert!(!paths.contains(&format!("{dir}/")));
    let body = "must stay inside the shared root";
    let resp = with_new_upload_headers(
        server.request(Method::PUT, format!("{}{dir}/created.txt", server.url())),
        body.len() as u64,
    )
    .body(body)
    .send()?;
    assert_eq!(resp.status(), 404);
    assert!(!tmpdir.path().join("created.txt").exists());
    Ok(())
}

#[rstest]
fn symlink_that_stays_inside_root_is_served(server: TestServer) -> Result<(), Error> {
    let dir = "inside";
    symlink("dir1", server.path().join(dir)).expect("Couldn't create symlink");
    let resp = server.get(format!("{}{dir}/index.html", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text()?, "This is dir1/index.html");
    let resp = server.get(server.url())?;
    let paths = server.paths_from_page(resp)?;
    assert!(paths.contains(&format!("{dir}/")));
    Ok(())
}

#[rstest]
fn absolute_symlink_is_not_served_even_if_target_is_inside_root(
    server: TestServer,
) -> Result<(), Error> {
    let dir = "absolute";
    symlink(server.path().join("dir1"), server.path().join(dir)).expect("Couldn't create symlink");

    let resp = server.get(format!("{}{dir}/index.html", server.url()))?;
    assert_eq!(resp.status(), 404);
    let resp = server.get(server.url())?;
    let paths = server.paths_from_page(resp)?;
    assert!(!paths.contains(&format!("{dir}/")));

    Ok(())
}

#[rstest]
fn dangling_and_looping_symlinks_are_listed_and_manageable(
    server: TestServer,
) -> Result<(), Error> {
    let dangling = server.path().join("dangling-link");
    let looping = server.path().join("looping-link");
    symlink("missing-target", &dangling)?;
    symlink("looping-link", &looping)?;

    let response = server.get(server.url())?;
    assert_eq!(response.status(), 200);
    let paths = server.paths_from_page(response)?;
    assert!(paths.contains("dangling-link"));
    assert!(paths.contains("looping-link"));

    let dangling_url = server.url().join("dangling-link")?;
    assert_eq!(server.get(dangling_url.clone())?.status(), 404);
    let deleted = server.request(Method::DELETE, dangling_url).send()?;
    assert_eq!(deleted.status(), 204);
    assert!(std::fs::symlink_metadata(&dangling).is_err());

    let preflight = preflight_upload_target(&server, "/looping-link")?;
    assert!(preflight.exists);
    assert!(preflight.replaceable);
    let revision = preflight
        .revision
        .ok_or("looping symlink has no target revision")?;
    let refused_body = "must not write";
    let refused = with_new_upload_headers(
        server.request(Method::PUT, server.url().join("looping-link")?),
        refused_body.len() as u64,
    )
    .body(refused_body)
    .send()?;
    assert_eq!(refused.status(), 409);
    assert_eq!(
        refused.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    let problem: Value = serde_json::from_str(&refused.text()?)?;
    assert_eq!(problem["code"], "destination_exists");
    assert!(
        std::fs::symlink_metadata(&looping)?
            .file_type()
            .is_symlink()
    );

    let replacement = "replaced link";
    let replaced = with_new_upload_overwrite_headers(
        server.request(Method::PUT, server.url().join("looping-link")?),
        replacement.len() as u64,
        &revision,
    )
    .body(replacement)
    .send()?;
    assert_eq!(replaced.status(), 201);
    assert_eq!(std::fs::read_to_string(&looping)?, replacement);
    assert!(
        !std::fs::symlink_metadata(&looping)?
            .file_type()
            .is_symlink()
    );
    Ok(())
}

#[rstest]
fn upload_preflight_distinguishes_missing_paths_from_unresolvable_ancestors(
    server: TestServer,
    tmpdir: TempDir,
) -> Result<(), Error> {
    let missing = preflight_upload_target(&server, "/new-parent/new-child.txt")?;
    assert!(!missing.exists);
    assert!(missing.replaceable);
    assert!(missing.revision.is_none());

    symlink("missing-target", server.path().join("dangling-parent"))?;
    symlink("loop-parent", server.path().join("loop-parent"))?;
    symlink(tmpdir.path(), server.path().join("outside-parent"))?;
    std::fs::write(server.path().join("file-parent"), b"not a directory")?;

    for path in [
        "/dangling-parent/child.txt",
        "/loop-parent/child.txt",
        "/outside-parent/child.txt",
        "/file-parent/child.txt",
    ] {
        assert_invalid_preflight_path(&server, path)?;
    }
    Ok(())
}

#[rstest]
fn running_server_remains_anchored_to_the_original_root(server: TestServer) -> Result<(), Error> {
    let root = server.path().to_path_buf();
    let moved_root = root.with_extension("opened-root");
    std::fs::rename(&root, &moved_root)?;
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("replacement-only.txt"), "wrong root")?;

    let result = (|| -> Result<(), Error> {
        let response = server.list_api("/", &[])?;
        assert_eq!(response.status(), 200);
        let data: Value = serde_json::from_str(&response.text()?)?;
        let names = data["paths"]
            .as_array()
            .ok_or("List response has no paths")?
            .iter()
            .filter_map(|item| item["name"].as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"test.html"));
        assert!(!names.contains(&"replacement-only.txt"));

        let original = server.get(server.url().join("test.html")?)?;
        assert_eq!(original.status(), 200);
        assert_eq!(original.text()?, "This is test.html");
        let replacement = server.get(server.url().join("replacement-only.txt")?)?;
        assert_eq!(replacement.status(), 404);
        Ok(())
    })();

    std::fs::remove_dir_all(&root)?;
    std::fs::rename(&moved_root, &root)?;
    result
}
