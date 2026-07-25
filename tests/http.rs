mod fixtures;
mod utils;

use fixtures::{
    BIN_FILE, Error, TestServer, server, with_new_upload_headers, with_resume_upload_headers,
    with_upload_headers,
};
use rstest::rstest;
use sha2::{Digest, Sha256};
use std::{ffi::OsString, os::unix::ffi::OsStringExt};
use uuid::Uuid;

#[rstest]
fn get_dir(server: TestServer) -> Result<(), Error> {
    let resp = server.get(server.url())?;
    assert!(resp.headers().contains_key("content-length"));
    assert_resp_paths!(server, resp);
    Ok(())
}

#[rstest]
fn head_dir(server: TestServer) -> Result<(), Error> {
    let resp = server.request(reqwest::Method::HEAD, server.url()).send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    assert!(!resp.headers().contains_key("content-length"));
    assert!(resp.headers().contains_key("cache-control"));
    assert!(resp.headers().contains_key("content-security-policy"));
    assert!(resp.headers().contains_key("x-content-type-options"));
    assert!(resp.headers().contains_key("x-frame-options"));
    assert!(resp.headers().contains_key("referrer-policy"));
    assert!(resp.headers().contains_key("permissions-policy"));
    assert_eq!(resp.text()?, "");
    Ok(())
}

#[rstest]
fn get_missing_dir_shows_upload_target(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}404/", server.url()))?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn head_missing_dir_shows_upload_target(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}404/", server.url()))
        .send()?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
#[case(server(&[] as &[&str]))]
#[case(server(&["--compress", "none"]))]
#[case(server(&["--compress", "low"]))]
#[case(server(&["--compress", "medium"]))]
#[case(server(&["--compress", "high"]))]
fn get_dir_zip(#[case] server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?zip", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/zip"
    );
    assert!(resp.headers().contains_key("content-disposition"));
    let content_length = resp
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or("Missing ZIP Content-Length")?;
    let body = resp.bytes()?;
    assert_eq!(body.len(), content_length);
    assert!(body.starts_with(b"PK\x03\x04"));
    assert!(
        body.windows(4).any(|window| window == b"PK\x05\x06"),
        "ZIP must contain a finalized end-of-central-directory record"
    );
    Ok(())
}

#[rstest]
fn unknown_directory_query_is_ignored(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?unused=1", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    let paths = server.paths_from_page(resp)?;
    assert!(paths.contains("index.html"));
    Ok(())
}

#[rstest]
fn head_dir_zip(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}?zip", server.url()))
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/zip"
    );
    assert!(resp.headers().contains_key("content-disposition"));
    assert!(!resp.headers().contains_key("content-length"));
    assert_eq!(resp.text()?, "");
    Ok(())
}

#[rstest]
fn non_utf8_names_are_rejected_without_partial_browser_or_zip_results(
    server: TestServer,
) -> Result<(), Error> {
    const MESSAGE: &str = "目录包含不受支持的非 UTF-8 名称，请在 Linux 中重命名";

    let directory = server.path().join("utf8-policy");
    std::fs::create_dir(&directory)?;
    std::fs::write(directory.join("visible.txt"), b"visible")?;
    let invalid_name = OsString::from_vec(b"invalid-\xff.txt".to_vec());
    let invalid_path = directory.join(&invalid_name);
    std::fs::write(&invalid_path, b"must not become an empty browser path")?;

    let head = server
        .request(
            reqwest::Method::HEAD,
            format!("{}utf8-policy/?zip", server.url()),
        )
        .send()?;
    assert_eq!(head.status(), 200);
    assert_eq!(
        head.headers().get("content-type").unwrap(),
        "application/zip"
    );
    assert!(head.headers().contains_key("content-disposition"));
    assert!(!head.headers().contains_key("content-length"));
    assert_eq!(head.text()?, "");

    for response in [
        server.list_api("/utf8-policy", &[])?,
        server.list_api("/utf8-policy", &[("q", "visible")])?,
        server.get(format!("{}utf8-policy/?zip", server.url()))?,
    ] {
        assert_eq!(response.status(), 409);
        assert_ne!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/zip")
        );
        assert!(response.headers().get("content-disposition").is_none());
        assert_eq!(response.text()?, MESSAGE);
    }

    assert!(directory.is_dir());
    assert!(invalid_path.is_file());
    assert_eq!(std::fs::read(directory.join("visible.txt"))?, b"visible");
    Ok(())
}

#[rstest]
fn recursive_walk_errors_do_not_return_partial_search_or_zip_success(
    server: TestServer,
) -> Result<(), Error> {
    let directory = server.path().join("walk-error");
    std::fs::create_dir(&directory)?;
    std::fs::write(directory.join("partial-match.txt"), b"partial")?;
    std::os::unix::fs::symlink(".", directory.join("loop"))?;

    let search = server.list_api("/walk-error", &[("q", "partial")])?;
    assert_eq!(search.status(), 500);
    assert_eq!(search.text()?, "Directory operation failed");

    let archive = server.get(format!("{}walk-error/?zip", server.url()))?;
    assert_eq!(archive.status(), 500);
    assert!(archive.headers().get("content-disposition").is_none());
    assert_ne!(
        archive
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/zip")
    );
    assert_eq!(archive.text()?, "Directory operation failed");
    Ok(())
}

#[rstest]
fn recursive_walk_budget_bounds_one_large_directory_before_full_collection(
    #[with(&[
        "--max-search-entries",
        "4",
        "--max-zip-entries",
        "4",
    ])]
    server: TestServer,
) -> Result<(), Error> {
    const CREATED: usize = 128;
    const MESSAGE: &str = "Directory operation exceeded its entry limit";

    let directory = server.path().join("bounded-walk");
    std::fs::create_dir(&directory)?;
    for index in 0..CREATED {
        std::fs::write(
            directory.join(format!("entry-{index:03}.txt")),
            index.to_string(),
        )?;
    }

    let search = server.list_api("/bounded-walk", &[("q", "entry")])?;
    assert_eq!(search.status(), 413);
    assert_eq!(search.text()?, MESSAGE);

    let archive = server.get(format!("{}bounded-walk/?zip", server.url()))?;
    assert_eq!(archive.status(), 413);
    assert!(archive.headers().get("content-disposition").is_none());
    assert_ne!(
        archive
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/zip")
    );
    assert_eq!(archive.text()?, MESSAGE);
    Ok(())
}

#[rstest]
fn get_dir_search(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q={}", server.url(), "test.html"))?;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("content-length"));
    let paths = server.paths_from_page(resp)?;
    assert!(!paths.is_empty());
    for p in paths {
        assert!(p.contains("test.html"));
    }
    Ok(())
}

#[rstest]
fn get_dir_search2(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q={BIN_FILE}", server.url()))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(!paths.is_empty());
    for p in paths {
        assert!(p.contains(BIN_FILE));
    }
    Ok(())
}

#[rstest]
fn get_dir_search3(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q={}", server.url(), "test.html"))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(paths.iter().any(|path| path.contains("test.html")));
    Ok(())
}

#[rstest]
fn get_dir_search4(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}dir1?q=dir1", server.url()))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(paths.is_empty());
    Ok(())
}

#[rstest]
fn head_dir_search(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(
            reqwest::Method::HEAD,
            format!("{}?q={}", server.url(), "test.html"),
        )
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    assert!(!resp.headers().contains_key("content-length"));
    assert!(resp.headers().contains_key("cache-control"));
    assert!(resp.headers().contains_key("content-security-policy"));
    assert!(resp.headers().contains_key("x-content-type-options"));
    assert!(resp.headers().contains_key("x-frame-options"));
    assert!(resp.headers().contains_key("referrer-policy"));
    assert!(resp.headers().contains_key("permissions-policy"));
    assert_eq!(resp.text()?, "");
    Ok(())
}

#[rstest]
fn empty_search(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q=", server.url()))?;
    assert_resp_paths!(server, resp);
    Ok(())
}

#[rstest]
fn get_file(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}index.html", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=UTF-8"
    );
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert!(resp.headers().contains_key("etag"));
    assert!(resp.headers().contains_key("last-modified"));
    assert!(resp.headers().contains_key("content-length"));
    assert_eq!(resp.text()?, "This is index.html");
    Ok(())
}

#[rstest]
fn head_file(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}index.html", server.url()))
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=UTF-8"
    );
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert!(resp.headers().contains_key("content-disposition"));
    assert!(resp.headers().contains_key("etag"));
    assert!(resp.headers().contains_key("last-modified"));
    assert!(resp.headers().contains_key("content-length"));
    assert_eq!(resp.text()?, "");
    Ok(())
}

#[rstest]
fn get_file_404(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}404", server.url()))?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn get_file_emoji_path(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}{BIN_FILE}", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"download\"; filename*=UTF-8''%F0%9F%98%80.bin"
    );
    Ok(())
}

#[rstest]
fn get_file_newline_path(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}file%0A1.txt", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"download\"; filename*=UTF-8''file%201.txt"
    );
    Ok(())
}

#[rstest]
fn content_disposition_safely_encodes_special_filenames(server: TestServer) -> Result<(), Error> {
    let cases = [
        (
            "quote\"name.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''quote%22name.txt",
        ),
        (
            "back\\slash.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''back%5Cslash.txt",
        ),
        (
            "semi;colon.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''semi%3Bcolon.txt",
        ),
        (
            "percent%name.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''percent%25name.txt",
        ),
        (
            "空 格.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''%E7%A9%BA%20%E6%A0%BC.txt",
        ),
        (
            "tab\tname.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''tab%20name.txt",
        ),
    ];

    for (filename, expected) in cases {
        std::fs::write(server.path().join(filename), b"contents")?;
        let resp = server.get(format!("{}{}", server.url(), utils::encode_uri(filename)))?;
        assert_eq!(resp.status(), 200, "unexpected status for {filename:?}");
        assert_eq!(
            resp.headers().get("content-disposition").unwrap(),
            expected,
            "unexpected Content-Disposition for {filename:?}",
        );
    }
    Ok(())
}

#[rstest]
fn directory_zip_uses_safe_content_disposition(server: TestServer) -> Result<(), Error> {
    let directory = "资料\"包";
    std::fs::create_dir(server.path().join(directory))?;
    let resp = server.get(format!(
        "{}{}/?zip",
        server.url(),
        utils::encode_uri(directory)
    ))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        concat!(
            "attachment; filename=\"archive.zip\"; ",
            "filename*=UTF-8''%E8%B5%84%E6%96%99%22%E5%8C%85.zip"
        )
    );
    Ok(())
}

#[rstest]
fn unknown_file_query_still_returns_a_download(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(
            reqwest::Method::GET,
            format!("{}index.html?unused=1", server.url()),
        )
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"download\"; filename*=UTF-8''index.html"
    );
    Ok(())
}

#[rstest]
fn head_file_404(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}404", server.url()))
        .send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn put_file(server: TestServer) -> Result<(), Error> {
    let url = format!("{}file1", server.url());
    let resp = with_new_upload_headers(server.request(reqwest::Method::PUT, &url), 3)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 201);
    let resp = server.get(url)?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn upload_rejects_configured_and_declared_length_overflow(
    #[with(&["--max-upload-size", "3", "--min-free-space", "0"])] server: TestServer,
) -> Result<(), Error> {
    let target = server.path().join("index.html");
    let original = std::fs::read(&target)?;
    let url = format!("{}index.html", server.url());

    let over_limit = with_new_upload_headers(server.request(reqwest::Method::PUT, &url), 4)
        .body(b"four".to_vec())
        .send()?;
    assert_eq!(over_limit.status(), 413);
    assert_eq!(std::fs::read(&target)?, original);

    let excess_body = with_new_upload_headers(server.request(reqwest::Method::PUT, &url), 3)
        .body(b"abcdef".to_vec())
        .send()?;
    assert_eq!(excess_body.status(), 413);
    assert_eq!(std::fs::read(&target)?, original);
    assert!(
        !std::fs::read_dir(server.path())?
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".dufs-upload-"))
    );
    Ok(())
}

#[rstest]
fn upload_preserves_the_configured_free_space_floor(
    #[with(&[
        "--min-free-space",
        "18446744073709551615",
        "--max-upload-size",
        "1024"
    ])]
    server: TestServer,
) -> Result<(), Error> {
    let target = server.path().join("index.html");
    let original = std::fs::read(&target)?;
    let response = with_new_upload_headers(
        server.request(reqwest::Method::PUT, format!("{}index.html", server.url())),
        1,
    )
    .body(b"x".to_vec())
    .send()?;

    assert_eq!(response.status(), 507);
    assert_eq!(std::fs::read(&target)?, original);
    Ok(())
}

#[rstest]
fn upload_concurrency_and_idle_time_are_bounded(
    #[with(&[
        "--max-concurrent-uploads",
        "1",
        "--upload-idle-timeout",
        "1",
        "--upload-total-timeout",
        "10",
        "--min-free-space",
        "0"
    ])]
    server: TestServer,
) -> Result<(), Error> {
    use fixtures::{TEST_PASSWORD, TEST_USER};
    use std::{
        io::{Read, Write},
        net::TcpStream,
        time::{Duration, Instant},
    };

    let session = server.login(TEST_USER, TEST_PASSWORD)?;
    let upload_id = Uuid::new_v4();
    let mut first = TcpStream::connect(("127.0.0.1", server.port()))?;
    first.set_read_timeout(Some(Duration::from_secs(5)))?;
    first.write_all(
        format!(
            concat!(
                "PUT /bounded-first.txt HTTP/1.1\r\n",
                "Host: localhost:{}\r\n",
                "Cookie: {}\r\n",
                "X-Dufs-CSRF-Token: {}\r\n",
                "X-Dufs-Upload-Id: {}\r\n",
                "X-Dufs-Upload-Length: 6\r\n",
                "Content-Length: 6\r\n",
                "Connection: close\r\n",
                "\r\n",
                "abc"
            ),
            server.port(),
            session.cookie(),
            session.csrf_token(),
            upload_id,
        )
        .as_bytes(),
    )?;
    first.flush()?;

    let start = Instant::now();
    while !std::fs::read_dir(server.path())?
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".dufs-upload-")
        })
    {
        if start.elapsed() > Duration::from_secs(5) {
            return Err("first upload did not acquire its slot".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let rejected = with_new_upload_headers(
        server.request(
            reqwest::Method::PUT,
            format!("{}bounded-second.txt", server.url()),
        ),
        3,
    )
    .body(b"123".to_vec())
    .send()?;
    assert_eq!(rejected.status(), 429);
    assert_eq!(rejected.headers().get("retry-after").unwrap(), "1");
    assert!(!server.path().join("bounded-second.txt").exists());

    let mut first_response = String::new();
    first.read_to_string(&mut first_response)?;
    assert!(first_response.starts_with("HTTP/1.1 408"));
    assert!(!server.path().join("bounded-first.txt").exists());
    Ok(())
}

#[rstest]
fn put_overwrite_replaces_inode_preserves_mode_and_leaves_hardlinks_unchanged(
    server: TestServer,
) -> Result<(), Error> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let target = server.path().join("index.html");
    let hardlink = server.path().join("index-hardlink.html");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640))?;
    std::fs::hard_link(&target, &hardlink)?;
    let old_inode = std::fs::metadata(&target)?.ino();

    let resp = with_new_upload_headers(
        server.request(reqwest::Method::PUT, format!("{}index.html", server.url())),
        11,
    )
    .body(b"replacement".to_vec())
    .send()?;
    assert_eq!(resp.status(), 201);

    let metadata = std::fs::metadata(&target)?;
    assert_ne!(metadata.ino(), old_inode);
    assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
    assert_eq!(std::fs::read_to_string(&target)?, "replacement");
    assert_eq!(std::fs::read_to_string(&hardlink)?, "This is index.html");
    Ok(())
}

#[rstest]
fn put_file_create_dir(server: TestServer) -> Result<(), Error> {
    let url = format!("{}xyz/file1", server.url());
    let resp = with_new_upload_headers(server.request(reqwest::Method::PUT, &url), 3)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 201);
    let resp = server.get(url)?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn put_file_create_deep_dir(server: TestServer) -> Result<(), Error> {
    let url = format!("{}newdir/subdir/file1", server.url());
    let resp = with_new_upload_headers(server.request(reqwest::Method::PUT, &url), 3)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 201);
    let resp = server.get(url)?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn put_file_conflict_dir(server: TestServer) -> Result<(), Error> {
    let url = format!("{}dir1", server.url());
    let resp = with_new_upload_headers(server.request(reqwest::Method::PUT, &url), 3)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text()?, "Target is a directory");
    Ok(())
}

#[rstest]
fn delete_file(server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.html", server.url());
    let resp = server.request(reqwest::Method::DELETE, &url).send()?;
    assert_eq!(resp.status(), 204);
    let resp = server.get(url)?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn delete_file_404(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::DELETE, format!("{}file1", server.url()))
        .send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn delete_root_is_forbidden_and_preserves_contents(server: TestServer) -> Result<(), Error> {
    let root_url = server.url();
    let unauthenticated = server
        .raw_request(reqwest::Method::DELETE, root_url.clone())
        .send()?;
    assert_eq!(unauthenticated.status(), 401);

    for suffix in ["", "/", "%2F", "?root-delete"] {
        let resp = server
            .request(reqwest::Method::DELETE, format!("{root_url}{suffix}"))
            .send()?;
        assert_eq!(resp.status(), 403, "suffix={suffix}");
        assert!(server.path().is_dir(), "suffix={suffix}");
        assert_eq!(
            std::fs::read_to_string(server.path().join("test.html"))?,
            "This is test.html",
            "suffix={suffix}"
        );
    }

    assert_eq!(server.get(root_url)?.status(), 200);
    Ok(())
}

#[rstest]
fn delete_prefixed_root_equivalents_are_forbidden(
    #[with(&["--path-prefix", "xyz"])] server: TestServer,
) -> Result<(), Error> {
    for path in ["xyz", "xyz/", "xyz//", "%78yz/", "xyz/%2F"] {
        let resp = server
            .request(reqwest::Method::DELETE, format!("{}{path}", server.url()))
            .send()?;
        assert_eq!(resp.status(), 403, "path={path}");
        assert!(server.path().is_dir(), "path={path}");
        assert!(server.path().join("test.html").is_file(), "path={path}");
    }

    assert_eq!(server.get(format!("{}xyz/", server.url()))?.status(), 200);
    Ok(())
}

#[rstest]
fn delete_child_directory_still_succeeds(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::DELETE, format!("{}dir1/", server.url()))
        .send()?;
    assert_eq!(resp.status(), 204);
    assert!(!server.path().join("dir1").exists());
    assert!(server.path().is_dir());
    assert!(server.path().join("test.html").is_file());
    Ok(())
}

#[rstest]
fn delete_ancestor_waits_for_active_upload(server: TestServer) -> Result<(), Error> {
    use fixtures::{TEST_PASSWORD, TEST_USER};
    use std::{
        io::Write,
        net::TcpStream,
        sync::mpsc,
        time::{Duration, Instant},
    };

    let session = server.login(TEST_USER, TEST_PASSWORD)?;
    let upload_id = Uuid::new_v4();
    let mut upload = TcpStream::connect(("127.0.0.1", server.port()))?;
    upload.write_all(
        format!(
            concat!(
                "PUT /locked/file.txt HTTP/1.1\r\n",
                "Host: localhost:{}\r\n",
                "Cookie: {}\r\n",
                "X-Dufs-CSRF-Token: {}\r\n",
                "X-Dufs-Upload-Id: {}\r\n",
                "X-Dufs-Upload-Length: 6\r\n",
                "Content-Length: 6\r\n",
                "Connection: close\r\n",
                "\r\n",
                "abc"
            ),
            server.port(),
            session.cookie(),
            session.csrf_token(),
            upload_id
        )
        .as_bytes(),
    )?;
    upload.flush()?;

    let upload_dir = server.path().join("locked");
    let start = Instant::now();
    while !std::fs::read_dir(&upload_dir).ok().is_some_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".dufs-upload-")
        })
    }) {
        if start.elapsed() > Duration::from_secs(5) {
            return Err("upload staging file was not created".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let delete_url = format!("{}locked/", server.url());
    let cookie = session.cookie().to_owned();
    let csrf = session.csrf_token().to_owned();
    let (delete_tx, delete_rx) = mpsc::channel();
    let delete_thread = std::thread::spawn(move || {
        let result = reqwest::blocking::Client::new()
            .delete(delete_url)
            .header("cookie", cookie)
            .header("x-dufs-csrf-token", csrf)
            .send()
            .map(|response| response.status());
        let _ = delete_tx.send(result);
    });

    assert!(
        delete_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "ancestor delete completed while its descendant upload was active"
    );

    upload.write_all(b"123")?;
    upload.flush()?;

    let delete_status = delete_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "ancestor delete did not resume after upload commit")??;
    delete_thread.join().unwrap();
    drop(upload);
    assert_eq!(delete_status, reqwest::StatusCode::NO_CONTENT);
    assert!(!upload_dir.exists());
    Ok(())
}

#[rstest]
fn get_file_content_type(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}content-types/bin.tar", server.url()))?;
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/x-tar"
    );
    let resp = server.get(format!("{}content-types/bin", server.url()))?;
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    let resp = server.get(format!("{}content-types/file-utf8.txt", server.url()))?;
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/plain; charset=UTF-8"
    );
    let resp = server.get(format!("{}content-types/file-gbk.txt", server.url()))?;
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/plain; charset=GBK"
    );
    let resp = server.get(format!("{}content-types/file", server.url()))?;
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/plain; charset=UTF-8"
    );
    Ok(())
}

#[rstest]
fn resumable_upload(server: TestServer) -> Result<(), Error> {
    let url = format!("{}file1", server.url());
    let upload_id = Uuid::new_v4();
    let resp = with_upload_headers(server.request(reqwest::Method::PUT, &url), upload_id, 6)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 409);
    let resp = with_resume_upload_headers(
        server.request(reqwest::Method::PATCH, &url),
        upload_id,
        6,
        3,
    )
    .body(b"123".to_vec())
    .send()?;
    assert_eq!(resp.status(), 204);
    let resp = server.get(url)?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().unwrap(), "abc123");
    Ok(())
}

#[rstest]
fn durable_resumable_upload_keeps_old_file_until_commit(
    mut server: TestServer,
) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let upload_id = Uuid::new_v4();

    let resp = with_upload_headers(server.request(reqwest::Method::PUT, &url), upload_id, 6)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );

    let resp = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-length").unwrap(), "3");
    assert_eq!(resp.headers().get("x-dufs-upload-offset").unwrap(), "3");
    assert_eq!(resp.headers().get("x-dufs-upload-length").unwrap(), "6");

    let staging_path = std::fs::read_dir(server.path())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".part"))
        })
        .expect("upload staging file");
    let mut staging_file = std::fs::OpenOptions::new()
        .append(true)
        .open(staging_path)?;
    std::io::Write::write_all(&mut staging_file, b"uncheckpointed")?;
    drop(staging_file);
    server.restart_with_default_auth();
    let url = format!("{}index.html", server.url());

    let resp = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-dufs-upload-offset").unwrap(), "3");

    let resp = with_resume_upload_headers(
        server.request(reqwest::Method::PATCH, &url),
        upload_id,
        6,
        3,
    )
    .body(b"123".to_vec())
    .send()?;
    assert_eq!(resp.status(), 204);
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "abc123"
    );
    assert!(
        !std::fs::read_dir(server.path())?
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".dufs-upload-"))
    );

    let resp = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn durable_upload_session_rejects_changed_total_length(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let upload_id = Uuid::new_v4();
    let resp = with_upload_headers(server.request(reqwest::Method::PUT, &url), upload_id, 6)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 409);

    let resp = with_resume_upload_headers(
        server.request(reqwest::Method::PATCH, &url),
        upload_id,
        4,
        3,
    )
    .body(b"d".to_vec())
    .send()?;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );

    let resp = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-dufs-upload-offset").unwrap(), "3");
    assert_eq!(resp.headers().get("x-dufs-upload-length").unwrap(), "6");
    Ok(())
}

#[rstest]
fn durable_upload_rejects_stage_shorter_than_checkpoint(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let upload_id = Uuid::new_v4();
    let resp = with_upload_headers(server.request(reqwest::Method::PUT, &url), upload_id, 6)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 409);

    let staging_path = std::fs::read_dir(server.path())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".part"))
        })
        .expect("upload staging file");
    std::fs::OpenOptions::new()
        .write(true)
        .open(staging_path)?
        .set_len(2)?;

    let resp = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(resp.status(), 404);
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );

    let resp = with_upload_headers(server.request(reqwest::Method::PUT, &url), upload_id, 6)
        .body(b"abc123".to_vec())
        .send()?;
    assert_eq!(resp.status(), 201);
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "abc123"
    );
    Ok(())
}

#[rstest]
fn durable_put_rejects_invalid_length_without_replacing_target(
    server: TestServer,
) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let upload_id = Uuid::new_v4();
    let resp = with_upload_headers(server.request(reqwest::Method::PUT, &url), upload_id, 7)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );
    Ok(())
}

#[rstest]
fn durable_upload_session_requires_total_length(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let upload_id = Uuid::new_v4();
    let resp = server
        .request(reqwest::Method::PUT, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .body(b"replacement".to_vec())
        .send()?;
    assert_eq!(resp.status(), 400);
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );

    let resp = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn patch_requires_current_upload_protocol_headers(server: TestServer) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let resp = server
        .request(reqwest::Method::PATCH, &url)
        .body(b"partial".to_vec())
        .send()?;
    assert_eq!(resp.status(), 400);
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );
    Ok(())
}

#[rstest]
fn upload_staging_files_are_hidden_and_not_addressable(server: TestServer) -> Result<(), Error> {
    let upload_id = Uuid::new_v4();
    let target_tag = hex::encode(Sha256::digest(b"target.txt"));
    let stage_name = format!(".dufs-upload-{target_tag}-{upload_id}.part");
    let staging_names = [
        stage_name.clone(),
        format!("{stage_name}.state"),
        format!("{stage_name}.state-{}.tmp", Uuid::new_v4()),
        format!(".dufs-upload-delete-{}.trash", Uuid::new_v4()),
    ];
    for staging_name in &staging_names {
        std::fs::write(server.path().join(staging_name), b"partial")?;
    }

    let resp = server.get(server.url())?;
    let paths = server.paths_from_page(resp)?;
    for staging_name in &staging_names {
        assert!(!paths.iter().any(|path| path.contains(staging_name)));
        let resp = server.get(format!("{}{}", server.url(), staging_name))?;
        assert_eq!(resp.status(), 400);
    }

    let ordinary_names = [
        ".dufs-upload-not-a-stage.part",
        ".dufs-upload-delete-old.trash",
    ];
    for ordinary_name in ordinary_names {
        std::fs::write(server.path().join(ordinary_name), b"ordinary")?;
    }
    let resp = server.get(server.url())?;
    let paths = server.paths_from_page(resp)?;
    for ordinary_name in ordinary_names {
        assert!(paths.iter().any(|path| path.contains(ordinary_name)));
        let resp = server.get(format!("{}{}", server.url(), ordinary_name))?;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.bytes()?.as_ref(), b"ordinary");
    }
    Ok(())
}
