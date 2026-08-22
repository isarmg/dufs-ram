use super::*;

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
fn full_running_checkpoint_can_reenter_commit_with_empty_patch(
    #[with(&[
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
        time::Duration,
    };

    const UPLOAD_LENGTH: usize = 20 * 1024 * 1024;

    let session = server.login(TEST_USER, TEST_PASSWORD)?;
    let upload_id = Uuid::new_v4();
    let mut upload = TcpStream::connect(("127.0.0.1", server.port()))?;
    upload.set_read_timeout(Some(Duration::from_secs(10)))?;
    upload.set_write_timeout(Some(Duration::from_secs(10)))?;
    upload.write_all(
        format!(
            concat!(
                "PUT /full-checkpoint.bin HTTP/1.1\r\n",
                "Host: localhost:{}\r\n",
                "Cookie: {}\r\n",
                "X-Dufs-CSRF-Token: {}\r\n",
                "X-Dufs-Upload-Id: {}\r\n",
                "X-Dufs-Upload-Length: {}\r\n",
                "Transfer-Encoding: chunked\r\n",
                "Connection: close\r\n",
                "\r\n",
                "{:x}\r\n"
            ),
            server.port(),
            session.cookie(),
            session.csrf_token(),
            upload_id,
            UPLOAD_LENGTH,
            UPLOAD_LENGTH,
        )
        .as_bytes(),
    )?;
    upload.write_all(&vec![b'x'; UPLOAD_LENGTH])?;
    upload.write_all(b"\r\n")?;
    upload.flush()?;

    // The complete chunk is durable, but withholding the terminating zero
    // chunk forces the request through the timeout checkpoint path.
    let mut timeout_response = String::new();
    upload.read_to_string(&mut timeout_response)?;
    assert!(
        timeout_response.starts_with("HTTP/1.1 408"),
        "{timeout_response}"
    );
    assert!(!server.path().join("full-checkpoint.bin").exists());

    let checkpoint = server
        .request(
            reqwest::Method::HEAD,
            server.url().join("full-checkpoint.bin")?,
        )
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(checkpoint.status(), 200);
    assert_eq!(
        checkpoint.headers().get("x-dufs-operation-state").unwrap(),
        "running"
    );
    assert_eq!(
        checkpoint.headers().get("x-dufs-upload-offset").unwrap(),
        UPLOAD_LENGTH.to_string().as_str()
    );

    let committed = with_resume_upload_headers(
        server.request(
            reqwest::Method::PATCH,
            server.url().join("full-checkpoint.bin")?,
        ),
        upload_id,
        UPLOAD_LENGTH as u64,
        UPLOAD_LENGTH as u64,
    )
    .body(Vec::new())
    .send()?;
    assert_eq!(committed.status(), 204);
    assert_eq!(
        committed.headers().get("x-dufs-operation-state").unwrap(),
        "committed"
    );
    let metadata = std::fs::metadata(server.path().join("full-checkpoint.bin"))?;
    assert_eq!(metadata.len(), UPLOAD_LENGTH as u64);
    Ok(())
}

#[rstest]
fn resuming_an_unknown_upload_returns_a_problem_with_protocol_headers(
    server: TestServer,
) -> Result<(), Error> {
    let url = format!("{}unknown-upload.bin", server.url());
    let upload_id = Uuid::new_v4();
    let response = with_resume_upload_headers(
        server.request(reqwest::Method::PATCH, &url),
        upload_id,
        3,
        0,
    )
    .body(b"abc".to_vec())
    .send()?;

    assert_eq!(response.status(), 404);
    assert_eq!(
        response.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    assert_eq!(
        response.headers().get("x-dufs-operation-state").unwrap(),
        "not-seen"
    );
    assert!(!response.headers().contains_key("x-dufs-upload-length"));
    assert!(!response.headers().contains_key("x-dufs-upload-offset"));
    assert_upload_problem_body(
        response,
        "upload_session_not_found",
        "Not Found",
        "retry_with_new_id",
    )?;
    assert!(!server.path().join("unknown-upload.bin").exists());
    Ok(())
}

#[rstest]
fn durable_resumable_upload_keeps_old_file_until_commit(
    mut server: TestServer,
) -> Result<(), Error> {
    let state_dir = assert_fs::TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let state_args = [
        OsString::from("--state-dir"),
        state_dir.path().as_os_str().to_owned(),
    ];
    server.restart_with_default_auth_args(state_args.clone());
    let url = format!("{}index.html", server.url());
    let upload_id = Uuid::new_v4();
    let preflight = preflight_upload_target(&server, "/index.html")?;
    let revision = preflight.revision.ok_or("existing file has no revision")?;

    let resp = with_upload_overwrite_headers(
        server.request(reqwest::Method::PUT, &url),
        upload_id,
        6,
        &revision,
    )
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
    server.restart_with_default_auth_args(state_args.clone());
    let url = format!("{}index.html", server.url());

    let resp = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-dufs-upload-offset").unwrap(), "3");

    let resp = with_upload_overwrite_headers(
        server.request(reqwest::Method::PATCH, &url),
        upload_id,
        6,
        &revision,
    )
    .header("X-Dufs-Upload-Offset", "3")
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
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
    );

    let resp = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-dufs-operation-state").unwrap(),
        "committed"
    );
    assert_eq!(
        resp.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    assert_eq!(resp.headers().get("x-dufs-upload-length").unwrap(), "6");
    assert_eq!(resp.headers().get("x-dufs-upload-offset").unwrap(), "6");

    let replay = with_upload_overwrite_headers(
        server.request(reqwest::Method::PUT, &url),
        upload_id,
        6,
        &revision,
    )
    .body(b"xxxxxx".to_vec())
    .send()?;
    assert_eq!(replay.status(), 200);
    assert_eq!(
        replay.headers().get("x-dufs-operation-state").unwrap(),
        "committed"
    );
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "abc123"
    );

    server.restart_with_default_auth_args(state_args);
    let url = format!("{}index.html", server.url());
    let after_restart = server
        .request(reqwest::Method::HEAD, &url)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(after_restart.status(), 200);
    assert_eq!(
        after_restart
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "committed"
    );
    Ok(())
}

#[rstest]
fn durable_upload_session_rejects_changed_total_length(server: TestServer) -> Result<(), Error> {
    let target = server.path().join("changed-total-length.bin");
    let url = format!("{}changed-total-length.bin", server.url());
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
    assert!(!target.exists());

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
    let target = server.path().join("short-checkpoint.bin");
    let url = format!("{}short-checkpoint.bin", server.url());
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
    assert!(!target.exists());

    let retry_upload_id = Uuid::new_v4();
    let resp = with_upload_headers(
        server.request(reqwest::Method::PUT, &url),
        retry_upload_id,
        6,
    )
    .body(b"abc123".to_vec())
    .send()?;
    assert_eq!(resp.status(), 201);
    assert_eq!(std::fs::read_to_string(target)?, "abc123");
    Ok(())
}

#[rstest]
fn durable_put_rejects_invalid_length_without_replacing_target(
    server: TestServer,
) -> Result<(), Error> {
    let url = format!("{}index.html", server.url());
    let revision = preflight_upload_target(&server, "/index.html")?
        .revision
        .ok_or("existing file has no revision")?;
    let upload_id = Uuid::new_v4();
    let resp = with_upload_overwrite_headers(
        server.request(reqwest::Method::PUT, &url),
        upload_id,
        7,
        &revision,
    )
    .body(b"abc".to_vec())
    .send()?;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.headers().get("x-dufs-operation-state").unwrap(),
        "running"
    );
    assert_eq!(resp.headers().get("x-dufs-upload-offset").unwrap(), "3");
    assert_problem_code(resp, "upload_length_mismatch")?;
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
    assert_problem_code(resp, "invalid_upload_length")?;
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
    assert_problem_code(resp, "invalid_upload_id")?;
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );
    Ok(())
}

#[rstest]
fn upload_staging_files_are_hidden_and_not_addressable(server: TestServer) -> Result<(), Error> {
    let upload_id = Uuid::new_v4();
    let target_tag = dufs::utils::encode_hex(Sha256::digest(b"target.txt"));
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
    let search_paths =
        server.paths_from_page(server.get(format!("{}?q=.dufs-upload", server.url()))?)?;
    for staging_name in &staging_names {
        assert!(!search_paths.iter().any(|path| path.contains(staging_name)));
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
