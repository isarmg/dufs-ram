use super::*;

#[rstest]
fn delete_file(server: TestServer) -> Result<(), Error> {
    let url = format!("{}test.html", server.url());
    let revision = preflight_upload_target(&server, "/test.html")?
        .revision
        .ok_or("delete target has no revision")?;
    let resp = server
        .request(reqwest::Method::DELETE, &url)
        .header("If-Match", format!("\"{revision}\""))
        .send()?;
    assert_eq!(resp.status(), 204);
    let resp = server.get(url)?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn delete_requires_a_source_revision(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::DELETE, format!("{}file1", server.url()))
        .send()?;
    assert_eq!(resp.status(), 428);
    Ok(())
}

#[rstest]
fn delete_rejects_a_stale_revision_and_preserves_the_current_file(
    server: TestServer,
) -> Result<(), Error> {
    let revision = preflight_upload_target(&server, "/test.html")?
        .revision
        .ok_or("delete target has no revision")?;
    let target = server.path().join("test.html");
    std::fs::write(&target, "replacement after listing")?;

    let response = server
        .request(
            reqwest::Method::DELETE,
            format!("{}test.html", server.url()),
        )
        .header("If-Match", format!("\"{revision}\""))
        .send()?;

    assert_eq!(response.status(), 412);
    let current_revision = response
        .headers()
        .get("x-dufs-source-revision")
        .ok_or("stale DELETE response has no current source revision")?
        .to_str()?;
    assert_ne!(current_revision, revision);
    assert_eq!(
        std::fs::read_to_string(target)?,
        "replacement after listing"
    );
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
fn delete_child_directory_still_succeeds(server: TestServer) -> Result<(), Error> {
    let revision = preflight_upload_target(&server, "/dir1")?
        .revision
        .ok_or("delete directory has no revision")?;
    let resp = server
        .request(reqwest::Method::DELETE, format!("{}dir1/", server.url()))
        .header("If-Match", format!("\"{revision}\""))
        .send()?;
    assert_eq!(resp.status(), 204);
    assert!(!server.path().join("dir1").exists());
    assert!(server.path().is_dir());
    assert!(server.path().join("test.html").is_file());
    Ok(())
}

#[rstest]
fn delete_ancestor_waits_for_active_upload_then_rejects_its_stale_revision(
    server: TestServer,
) -> Result<(), Error> {
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
    let delete_revision = preflight_upload_target(&server, "/locked")?
        .revision
        .ok_or("active upload ancestor has no revision")?;

    let delete_url = format!("{}locked/", server.url());
    let cookie = session.cookie().to_owned();
    let csrf = session.csrf_token().to_owned();
    let (delete_tx, delete_rx) = mpsc::channel();
    let delete_thread = std::thread::spawn(move || {
        let result = reqwest::blocking::Client::new()
            .delete(delete_url)
            .header("cookie", cookie)
            .header("x-dufs-csrf-token", csrf)
            .header("x-dufs-operation-id", Uuid::new_v4().to_string())
            .header("if-match", format!("\"{delete_revision}\""))
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
    assert_eq!(delete_status, reqwest::StatusCode::PRECONDITION_FAILED);
    assert!(upload_dir.exists());
    assert_eq!(std::fs::read(upload_dir.join("file.txt"))?, b"abc123");
    Ok(())
}
