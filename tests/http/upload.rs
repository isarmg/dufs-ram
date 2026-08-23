use super::*;

#[rstest]
fn put_file(server: TestServer) -> Result<(), Error> {
    let url = format!("{}file1", server.url());
    let upload_id = Uuid::new_v4();
    let resp = with_upload_headers(server.request(reqwest::Method::PUT, &url), upload_id, 3)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 201);
    assert_eq!(
        resp.headers().get("x-dufs-operation-state").unwrap(),
        "committed"
    );
    assert_eq!(
        resp.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    assert_eq!(resp.headers().get("x-dufs-upload-length").unwrap(), "3");
    assert_eq!(resp.headers().get("x-dufs-upload-offset").unwrap(), "3");
    let resp = server.get(url)?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn new_uploads_are_published_with_owner_only_permissions(server: TestServer) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;

    let target = server.path().join("private-upload.txt");
    let response = with_new_upload_headers(
        server.request(
            reqwest::Method::PUT,
            format!("{}private-upload.txt", server.url()),
        ),
        7,
    )
    .body(b"private".to_vec())
    .send()?;

    assert_eq!(response.status(), 201);
    assert_eq!(
        std::fs::metadata(target)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[rstest]
fn repeated_upload_protocol_headers_are_rejected_over_http(
    server: TestServer,
) -> Result<(), Error> {
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let response = server
        .request(
            reqwest::Method::PUT,
            format!("{}duplicate-id.txt", server.url()),
        )
        .header("X-Dufs-Upload-Id", first_id.to_string())
        .header("X-Dufs-Upload-Id", second_id.to_string())
        .header("X-Dufs-Upload-Length", 1)
        .body(b"x".to_vec())
        .send()?;
    assert_eq!(response.status(), 400);
    assert_problem_code(response, "invalid_upload_id")?;
    assert!(!server.path().join("duplicate-id.txt").exists());

    let response = server
        .request(reqwest::Method::HEAD, server.url().join("index.html")?)
        .header("X-Dufs-Upload-Id", first_id.to_string())
        .header("X-Dufs-Upload-Id", second_id.to_string())
        .send()?;
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );

    let response = server
        .request(
            reqwest::Method::PUT,
            format!("{}duplicate-length.txt", server.url()),
        )
        .header("X-Dufs-Upload-Id", Uuid::new_v4().to_string())
        .header("X-Dufs-Upload-Length", 1)
        .header("X-Dufs-Upload-Length", 2)
        .body(b"x".to_vec())
        .send()?;
    assert_eq!(response.status(), 400);
    assert_problem_code(response, "invalid_upload_length")?;
    assert!(!server.path().join("duplicate-length.txt").exists());

    let response = server
        .request(
            reqwest::Method::PATCH,
            format!("{}duplicate-offset.txt", server.url()),
        )
        .header("X-Dufs-Upload-Id", Uuid::new_v4().to_string())
        .header("X-Dufs-Upload-Length", 1)
        .header("X-Dufs-Upload-Offset", 0)
        .header("X-Dufs-Upload-Offset", 1)
        .body(b"x".to_vec())
        .send()?;
    assert_eq!(response.status(), 400);
    assert_problem_code(response, "invalid_upload_offset")?;
    assert!(!server.path().join("duplicate-offset.txt").exists());
    Ok(())
}

#[rstest]
fn upload_rejects_configured_and_declared_length_overflow(
    #[with(&["--max-upload-size", "3", "--min-free-space", "0"])] server: TestServer,
) -> Result<(), Error> {
    let target = server.path().join("index.html");
    let original = std::fs::read(&target)?;
    let url = format!("{}index.html", server.url());
    let revision = preflight_upload_target(&server, "/index.html")?
        .revision
        .ok_or("existing file has no revision")?;

    let over_limit =
        with_new_upload_overwrite_headers(server.request(reqwest::Method::PUT, &url), 4, &revision)
            .body(b"four".to_vec())
            .send()?;
    assert_eq!(over_limit.status(), 413);
    assert_eq!(std::fs::read(&target)?, original);

    let excess_body =
        with_new_upload_overwrite_headers(server.request(reqwest::Method::PUT, &url), 3, &revision)
            .body(b"abcdef".to_vec())
            .send()?;
    assert_eq!(excess_body.status(), 413);
    assert_eq!(std::fs::read(&target)?, original);
    assert!(!server.path().join(UPLOAD_STAGE_DIRECTORY).exists());
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
    let preflight = preflight_upload_target(&server, "/index.html")?;
    let revision = preflight.revision.ok_or("existing file has no revision")?;
    let response = with_new_upload_overwrite_headers(
        server.request(reqwest::Method::PUT, format!("{}index.html", server.url())),
        1,
        &revision,
    )
    .body(b"x".to_vec())
    .send()?;

    assert_eq!(response.status(), 507);
    assert_eq!(std::fs::read(&target)?, original);
    Ok(())
}

#[rstest]
fn fresh_upload_rolls_back_new_ancestors_when_space_reservation_fails(
    #[with(&[
        "--min-free-space",
        "18446744073709551615",
        "--max-upload-size",
        "1024"
    ])]
    server: TestServer,
) -> Result<(), Error> {
    let response = with_new_upload_headers(
        server.request(
            reqwest::Method::PUT,
            server.url().join("new/deep/file.txt")?,
        ),
        1,
    )
    .body(b"x".to_vec())
    .send()?;

    assert_eq!(response.status(), 507);
    assert!(
        !server.path().join("new").exists(),
        "failed fresh PUT left newly-created ancestor directories behind"
    );
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
    while !std::fs::read_dir(server.path().join(UPLOAD_STAGE_DIRECTORY))
        .ok()
        .is_some_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dufs-upload-")
            })
        })
    {
        if start.elapsed() > Duration::from_secs(5) {
            return Err("first upload did not acquire its slot".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let second_upload_id = Uuid::new_v4();
    let rejected = with_upload_headers(
        server.request(
            reqwest::Method::PUT,
            format!("{}bounded-second.txt", server.url()),
        ),
        second_upload_id,
        3,
    )
    .body(b"123".to_vec())
    .send()?;
    assert_eq!(rejected.status(), 429);
    assert_eq!(rejected.headers().get("retry-after").unwrap(), "1");
    assert_eq!(
        rejected.headers().get("x-dufs-upload-id").unwrap(),
        second_upload_id.to_string().as_str()
    );
    assert_eq!(rejected.headers().get("x-dufs-upload-length").unwrap(), "3");
    assert_eq!(
        rejected.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    assert_upload_problem_body(
        rejected,
        "upload_concurrency_limit",
        "Too many concurrent uploads",
        "retry",
    )?;
    assert!(!server.path().join("bounded-second.txt").exists());

    let mut first_response = String::new();
    first.read_to_string(&mut first_response)?;
    assert!(first_response.starts_with("HTTP/1.1 408"));
    assert!(!server.path().join("bounded-first.txt").exists());
    Ok(())
}

#[rstest]
fn late_upload_conflict_keeps_the_stage_and_confirmation_reuses_it(
    #[with(&[
        "--upload-idle-timeout",
        "5",
        "--upload-total-timeout",
        "20",
        "--min-free-space",
        "0"
    ])]
    mut server: TestServer,
) -> Result<(), Error> {
    use fixtures::{TEST_PASSWORD, TEST_USER};
    use std::{
        io::{Read, Write},
        net::TcpStream,
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };

    let target = server.path().join("changed-during-upload.txt");
    assert!(!target.exists());
    let session = server.login(TEST_USER, TEST_PASSWORD)?;
    let upload_id = Uuid::new_v4();
    let mut upload = TcpStream::connect(("127.0.0.1", server.port()))?;
    upload.set_read_timeout(Some(Duration::from_secs(5)))?;
    upload.write_all(
        format!(
            concat!(
                "PUT /changed-during-upload.txt HTTP/1.1\r\n",
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
    upload.flush()?;

    let start = Instant::now();
    let stage = loop {
        let stage = std::fs::read_dir(server.path().join(UPLOAD_STAGE_DIRECTORY))
            .ok()
            .and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .find(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
            });
        if let Some(stage) = stage {
            break stage;
        }
        if start.elapsed() > Duration::from_secs(5) {
            return Err("upload staging file was not created".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(stage.metadata()?.permissions().mode() & 0o777, 0o600);

    // The preflight/admission observation was "missing", but another writer
    // wins the target name while this request is still transferring its body.
    std::fs::write(&target, b"competitor")?;
    upload.write_all(b"123")?;
    upload.flush()?;

    let mut response = String::new();
    upload.read_to_string(&mut response)?;
    assert!(response.starts_with("HTTP/1.1 409"), "{response}");
    assert!(
        response
            .to_ascii_lowercase()
            .contains("x-dufs-operation-state: awaiting-confirmation"),
        "{response}"
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("x-dufs-upload-offset: 6"),
        "{response}"
    );
    assert!(response.contains("\"code\":\"destination_exists\""));
    let first_revision = response
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("x-dufs-target-revision")
                .then(|| value.trim().to_string())
        })
        .ok_or("late conflict response is missing its target revision")?;
    assert_eq!(std::fs::read(&target)?, b"competitor");
    assert_eq!(std::fs::read(stage.path())?, b"abc123");

    server.restart_with_default_auth_args([
        "--upload-idle-timeout",
        "1",
        "--upload-total-timeout",
        "20",
        "--max-upload-size",
        "5",
    ]);

    let awaiting = server
        .request(
            reqwest::Method::HEAD,
            format!("{}changed-during-upload.txt", server.url()),
        )
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(awaiting.status(), 409);
    assert_eq!(
        awaiting.headers().get("x-dufs-operation-state").unwrap(),
        "awaiting-confirmation"
    );
    assert_eq!(awaiting.headers().get("x-dufs-upload-length").unwrap(), "6");
    assert_eq!(awaiting.headers().get("x-dufs-upload-offset").unwrap(), "6");
    assert_eq!(
        awaiting.headers().get("x-dufs-target-revision").unwrap(),
        first_revision.as_str()
    );
    assert_eq!(
        awaiting.headers().get("x-dufs-target-replaceable").unwrap(),
        "true"
    );

    let changed_length = with_upload_headers(
        server.request(
            reqwest::Method::PATCH,
            format!("{}changed-during-upload.txt", server.url()),
        ),
        upload_id,
        7,
    )
    .header("X-Dufs-Upload-Offset", "6")
    .body(Vec::new())
    .send()?;
    assert_eq!(changed_length.status(), 409);
    assert_eq!(
        changed_length
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "awaiting-confirmation"
    );
    assert_eq!(
        changed_length
            .headers()
            .get("x-dufs-upload-length")
            .unwrap(),
        "6"
    );
    assert_eq!(
        changed_length
            .headers()
            .get("x-dufs-upload-offset")
            .unwrap(),
        "6"
    );
    assert_upload_problem_body(
        changed_length,
        "upload_length_changed",
        "Upload length changed: expected 6, received 7",
        "query_upload",
    )?;

    let changed_offset = with_upload_headers(
        server.request(
            reqwest::Method::PATCH,
            format!("{}changed-during-upload.txt", server.url()),
        ),
        upload_id,
        6,
    )
    .header("X-Dufs-Upload-Offset", "5")
    .body(Vec::new())
    .send()?;
    assert_eq!(changed_offset.status(), 409);
    assert_eq!(
        changed_offset
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "awaiting-confirmation"
    );
    assert_eq!(
        changed_offset
            .headers()
            .get("x-dufs-upload-length")
            .unwrap(),
        "6"
    );
    assert_eq!(
        changed_offset
            .headers()
            .get("x-dufs-upload-offset")
            .unwrap(),
        "6"
    );
    assert_upload_problem_body(
        changed_offset,
        "upload_offset_changed",
        "Upload offset changed; query it again",
        "query_upload",
    )?;

    let nonempty_confirmation = with_upload_overwrite_headers(
        server.request(
            reqwest::Method::PATCH,
            format!("{}changed-during-upload.txt", server.url()),
        ),
        upload_id,
        6,
        &first_revision,
    )
    .header("X-Dufs-Upload-Offset", "6")
    .body(b"x".to_vec())
    .send()?;
    assert_eq!(nonempty_confirmation.status(), 413);
    assert_eq!(
        nonempty_confirmation
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "awaiting-confirmation"
    );
    assert_eq!(
        nonempty_confirmation
            .headers()
            .get("x-dufs-upload-length")
            .unwrap(),
        "6"
    );
    assert_eq!(
        nonempty_confirmation
            .headers()
            .get("x-dufs-upload-offset")
            .unwrap(),
        "6"
    );
    assert_upload_problem_body(
        nonempty_confirmation,
        "upload_body_exceeds_remaining_length",
        "Request body exceeds declared remaining upload length",
        "query_upload",
    )?;

    assert_eq!(std::fs::read(&target)?, b"competitor");
    assert_eq!(std::fs::read(stage.path())?, b"abc123");

    // A second writer changes the target after the conflict prompt. Confirming
    // the stale revision must fail without discarding or retransmitting stage.
    std::fs::write(&target, b"new competitor")?;
    let stale_confirmation = with_upload_overwrite_headers(
        server.request(
            reqwest::Method::PATCH,
            format!("{}changed-during-upload.txt", server.url()),
        ),
        upload_id,
        6,
        &first_revision,
    )
    .header("X-Dufs-Upload-Offset", "6")
    .body(Vec::new())
    .send()?;
    assert_eq!(stale_confirmation.status(), 409);
    assert_eq!(
        stale_confirmation
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "awaiting-confirmation"
    );
    assert_eq!(
        stale_confirmation
            .headers()
            .get("x-dufs-upload-offset")
            .unwrap(),
        "6"
    );
    let second_revision = stale_confirmation
        .headers()
        .get("x-dufs-target-revision")
        .ok_or("repeated destination conflict is missing its revision")?
        .to_str()?
        .to_string();
    assert_ne!(second_revision, first_revision);
    assert_eq!(std::fs::read(&target)?, b"new competitor");
    assert_eq!(std::fs::read(stage.path())?, b"abc123");

    // If that newly observed target disappears before the next confirmation,
    // the stale overwrite token is rejected again, but the server can now
    // prove that publication no longer needs overwrite authority.
    std::fs::remove_file(&target)?;
    let now_missing = with_upload_overwrite_headers(
        server.request(
            reqwest::Method::PATCH,
            format!("{}changed-during-upload.txt", server.url()),
        ),
        upload_id,
        6,
        &second_revision,
    )
    .header("X-Dufs-Upload-Offset", "6")
    .body(Vec::new())
    .send()?;
    assert_eq!(now_missing.status(), 409);
    assert_eq!(
        now_missing.headers().get("x-dufs-operation-state").unwrap(),
        "awaiting-confirmation"
    );
    assert_eq!(
        now_missing
            .headers()
            .get("x-dufs-target-replaceable")
            .unwrap(),
        "true"
    );
    assert!(!now_missing.headers().contains_key("x-dufs-target-revision"));
    let problem: serde_json::Value = serde_json::from_str(&now_missing.text()?)?;
    assert_eq!(problem["code"], "upload_target_changed");
    assert!(!target.exists());
    assert_eq!(std::fs::read(stage.path())?, b"abc123");

    let chunked_session = server.login(TEST_USER, TEST_PASSWORD)?;
    let mut stalled_confirmation = TcpStream::connect(("127.0.0.1", server.port()))?;
    stalled_confirmation.set_read_timeout(Some(Duration::from_secs(5)))?;
    stalled_confirmation.write_all(
        format!(
            concat!(
                "PATCH /changed-during-upload.txt HTTP/1.1\r\n",
                "Host: localhost:{}\r\n",
                "Cookie: {}\r\n",
                "X-Dufs-CSRF-Token: {}\r\n",
                "X-Dufs-Upload-Id: {}\r\n",
                "X-Dufs-Upload-Length: 6\r\n",
                "X-Dufs-Upload-Offset: 6\r\n",
                "X-Dufs-Upload-Overwrite: false\r\n",
                "Transfer-Encoding: chunked\r\n",
                "Connection: close\r\n",
                "\r\n"
            ),
            server.port(),
            chunked_session.cookie(),
            chunked_session.csrf_token(),
            upload_id,
        )
        .as_bytes(),
    )?;
    stalled_confirmation.flush()?;
    let mut stalled_response = String::new();
    stalled_confirmation.read_to_string(&mut stalled_response)?;
    assert!(
        stalled_response.starts_with("HTTP/1.1 408"),
        "{stalled_response}"
    );
    assert!(
        stalled_response
            .to_ascii_lowercase()
            .contains("x-dufs-operation-state: awaiting-confirmation"),
        "{stalled_response}"
    );
    assert!(
        stalled_response
            .to_ascii_lowercase()
            .contains("x-dufs-upload-offset: 6"),
        "{stalled_response}"
    );
    assert!(
        stalled_response.contains("\"code\":\"upload_idle_timeout\""),
        "{stalled_response}"
    );
    assert!(
        stalled_response.contains("\"recovery\":\"query_upload\""),
        "{stalled_response}"
    );
    assert!(!target.exists());
    assert_eq!(std::fs::read(stage.path())?, b"abc123");

    let mut chunked_confirmation = TcpStream::connect(("127.0.0.1", server.port()))?;
    chunked_confirmation.set_read_timeout(Some(Duration::from_secs(5)))?;
    chunked_confirmation.write_all(
        format!(
            concat!(
                "PATCH /changed-during-upload.txt HTTP/1.1\r\n",
                "Host: localhost:{}\r\n",
                "Cookie: {}\r\n",
                "X-Dufs-CSRF-Token: {}\r\n",
                "X-Dufs-Upload-Id: {}\r\n",
                "X-Dufs-Upload-Length: 6\r\n",
                "X-Dufs-Upload-Offset: 6\r\n",
                "X-Dufs-Upload-Overwrite: false\r\n",
                "Transfer-Encoding: chunked\r\n",
                "Connection: close\r\n",
                "\r\n",
                "1\r\nx\r\n",
                "0\r\n\r\n"
            ),
            server.port(),
            chunked_session.cookie(),
            chunked_session.csrf_token(),
            upload_id,
        )
        .as_bytes(),
    )?;
    chunked_confirmation.flush()?;
    let mut chunked_response = String::new();
    chunked_confirmation.read_to_string(&mut chunked_response)?;
    assert!(
        chunked_response.starts_with("HTTP/1.1 413"),
        "{chunked_response}"
    );
    assert!(
        chunked_response
            .to_ascii_lowercase()
            .contains("x-dufs-operation-state: awaiting-confirmation"),
        "{chunked_response}"
    );
    assert!(
        chunked_response
            .to_ascii_lowercase()
            .contains("x-dufs-upload-offset: 6"),
        "{chunked_response}"
    );
    assert!(
        chunked_response.contains("\"code\":\"upload_body_exceeds_remaining_length\""),
        "{chunked_response}"
    );
    assert!(
        chunked_response.contains("\"recovery\":\"query_upload\""),
        "{chunked_response}"
    );
    assert!(!target.exists());
    assert_eq!(std::fs::read(stage.path())?, b"abc123");

    // A zero-byte, no-overwrite PATCH publishes the already durable six-byte
    // stage. Its success proves confirmation neither resends the selected file
    // body nor carries stale permission to replace a destination.
    let committed = with_upload_headers(
        server.request(
            reqwest::Method::PATCH,
            format!("{}changed-during-upload.txt", server.url()),
        ),
        upload_id,
        6,
    )
    .header("X-Dufs-Upload-Offset", "6")
    .header("X-Dufs-Upload-Overwrite", "false")
    .body(Vec::new())
    .send()?;
    assert_eq!(committed.status(), 204);
    assert_eq!(
        committed.headers().get("x-dufs-operation-state").unwrap(),
        "committed"
    );
    assert_eq!(std::fs::read(&target)?, b"abc123");
    assert!(!stage.path().exists());

    let terminal = server
        .request(
            reqwest::Method::HEAD,
            format!("{}changed-during-upload.txt", server.url()),
        )
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(terminal.status(), 200);
    assert_eq!(
        terminal.headers().get("x-dufs-operation-state").unwrap(),
        "committed"
    );
    Ok(())
}

#[rstest]
fn staged_existing_target_metadata_cannot_be_reused_as_a_create(
    #[with(&[
        "--upload-idle-timeout",
        "5",
        "--upload-total-timeout",
        "20",
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

    let logical_path = "/metadata-stage.txt";
    let target = server.path().join("metadata-stage.txt");
    std::fs::write(&target, b"original target")?;
    // Open the competing writer before making the target read-only. Unix
    // permission checks happen when the descriptor is opened, so this models
    // an external writer that was already active without requiring root.
    let mut competing_writer = std::fs::OpenOptions::new().write(true).open(&target)?;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o440))?;
    let preflight = preflight_upload_target(&server, logical_path)?;
    let original_revision = preflight
        .revision
        .ok_or("existing target preflight is missing its revision")?;
    let session = server.login(TEST_USER, TEST_PASSWORD)?;
    let upload_id = Uuid::new_v4();
    let mut upload = TcpStream::connect(("127.0.0.1", server.port()))?;
    upload.set_read_timeout(Some(Duration::from_secs(5)))?;
    upload.write_all(
        format!(
            concat!(
                "PUT /metadata-stage.txt HTTP/1.1\r\n",
                "Host: localhost:{}\r\n",
                "Cookie: {}\r\n",
                "X-Dufs-CSRF-Token: {}\r\n",
                "X-Dufs-Upload-Id: {}\r\n",
                "X-Dufs-Upload-Length: 6\r\n",
                "X-Dufs-Upload-Overwrite: true\r\n",
                "X-Dufs-Target-Revision: {}\r\n",
                "Content-Length: 6\r\n",
                "Connection: close\r\n",
                "\r\n",
                "abc"
            ),
            server.port(),
            session.cookie(),
            session.csrf_token(),
            upload_id,
            original_revision,
        )
        .as_bytes(),
    )?;
    upload.flush()?;

    let start = Instant::now();
    let stage = loop {
        let stage = std::fs::read_dir(server.path().join(UPLOAD_STAGE_DIRECTORY))
            .ok()
            .and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .find(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
            });
        if let Some(stage) = stage {
            break stage;
        }
        if start.elapsed() > Duration::from_secs(5) {
            return Err("metadata-preserving upload stage was not created".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    competing_writer.set_len(0)?;
    competing_writer.write_all(b"competing target")?;
    competing_writer.flush()?;
    upload.write_all(b"123")?;
    upload.flush()?;
    let mut response = String::new();
    upload.read_to_string(&mut response)?;
    assert!(response.starts_with("HTTP/1.1 409"), "{response}");
    assert!(response.contains("\"code\":\"destination_exists\""));
    assert!(
        response
            .to_ascii_lowercase()
            .contains("x-dufs-operation-state: awaiting-confirmation"),
        "{response}"
    );
    assert_eq!(std::fs::read(stage.path())?, b"abc123");
    assert_eq!(stage.metadata()?.permissions().mode() & 0o777, 0o440);
    assert_eq!(
        std::fs::metadata(stage.path().parent().unwrap())?
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "metadata replay must remain isolated behind an owner-only directory"
    );

    std::fs::remove_file(&target)?;
    let refused = with_upload_headers(
        server.request(
            reqwest::Method::PATCH,
            server.url().join("metadata-stage.txt")?,
        ),
        upload_id,
        6,
    )
    .header("X-Dufs-Upload-Offset", "6")
    .header("X-Dufs-Upload-Overwrite", "false")
    .body(Vec::new())
    .send()?;
    assert_eq!(refused.status(), 409);
    assert_eq!(
        refused.headers().get("x-dufs-operation-state").unwrap(),
        "awaiting-confirmation"
    );
    assert_eq!(refused.headers().get("x-dufs-upload-offset").unwrap(), "6");
    assert_problem_code(refused, "upload_metadata_preservation_refused")?;
    assert!(!target.exists());
    assert_eq!(std::fs::read(stage.path())?, b"abc123");

    let discarded = server
        .request(
            reqwest::Method::POST,
            server.url().join("__dufs__/api/upload/discard")?,
        )
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "path": logical_path,
                "upload_id": upload_id.to_string()
            })
            .to_string(),
        )
        .send()?;
    assert_eq!(discarded.status(), 204);
    assert_eq!(
        discarded.headers().get("x-dufs-operation-state").unwrap(),
        "rejected"
    );
    assert!(!stage.path().exists());

    let discard_retry = server
        .request(
            reqwest::Method::POST,
            server.url().join("__dufs__/api/upload/discard")?,
        )
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "path": logical_path,
                "upload_id": upload_id.to_string()
            })
            .to_string(),
        )
        .send()?;
    assert_eq!(discard_retry.status(), 204);
    assert_eq!(
        discard_retry.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    assert_eq!(
        discard_retry.headers().get("x-dufs-upload-length").unwrap(),
        "6"
    );
    assert_eq!(
        discard_retry.headers().get("x-dufs-upload-offset").unwrap(),
        "6"
    );
    assert_eq!(
        discard_retry
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "rejected"
    );
    assert!(!stage.path().exists());

    let rejected = server
        .request(
            reqwest::Method::HEAD,
            server.url().join("metadata-stage.txt")?,
        )
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(rejected.status(), 409);
    assert_eq!(
        rejected.headers().get("x-dufs-operation-state").unwrap(),
        "rejected"
    );
    assert!(!target.exists());
    Ok(())
}

#[rstest]
fn put_overwrite_refuses_to_break_hardlink_identity(server: TestServer) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;

    let target = server.path().join("index.html");
    let hardlink = server.path().join("index-hardlink.html");
    std::fs::hard_link(&target, &hardlink)?;
    let old_inode = std::fs::metadata(&target)?.ino();
    let upload_id = uuid::Uuid::new_v4();
    let preflight = preflight_upload_target(&server, "/index.html")?;
    assert!(preflight.exists);
    assert!(!preflight.replaceable);

    let resp = with_upload_headers(
        server.request(reqwest::Method::PUT, format!("{}index.html", server.url())),
        upload_id,
        11,
    )
    .body(b"replacement".to_vec())
    .send()?;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    assert_eq!(resp.headers().get("x-dufs-upload-length").unwrap(), "11");
    assert_eq!(
        resp.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    assert_problem_code(resp, "destination_exists")?;

    let metadata = std::fs::metadata(&target)?;
    assert_eq!(metadata.ino(), old_inode);
    assert_eq!(std::fs::metadata(&hardlink)?.ino(), old_inode);
    assert_eq!(std::fs::read_to_string(&target)?, "This is index.html");
    assert_eq!(std::fs::read_to_string(&hardlink)?, "This is index.html");
    Ok(())
}

#[rstest]
fn put_overwrite_rejects_a_fifo_without_waiting_for_a_peer(
    server: TestServer,
) -> Result<(), Error> {
    use rustix::fs::{Mode, mkfifoat};
    use std::{os::unix::fs::FileTypeExt, time::Duration};

    let root = std::fs::File::open(server.path())?;
    mkfifoat(&root, "named-pipe", Mode::RUSR | Mode::WUSR)?;
    let upload_id = uuid::Uuid::new_v4();
    let preflight = preflight_upload_target(&server, "/named-pipe")?;
    assert!(preflight.exists);
    assert!(!preflight.replaceable);

    let response = with_upload_headers(
        server.request(reqwest::Method::PUT, format!("{}named-pipe", server.url())),
        upload_id,
        1,
    )
    .timeout(Duration::from_secs(2))
    .body(b"x".to_vec())
    .send()?;

    assert_eq!(response.status(), 409);
    assert_eq!(
        response.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    assert_eq!(response.headers().get("x-dufs-upload-length").unwrap(), "1");
    assert_eq!(
        response.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    assert_problem_code(response, "destination_exists")?;
    assert!(
        std::fs::symlink_metadata(server.path().join("named-pipe"))?
            .file_type()
            .is_fifo()
    );
    Ok(())
}

#[rstest]
fn put_overwrite_rejects_a_unix_socket_without_opening_it(server: TestServer) -> Result<(), Error> {
    use std::{
        os::unix::{fs::FileTypeExt, net::UnixListener},
        time::Duration,
    };

    let socket_path = server.path().join("service.sock");
    let _listener = UnixListener::bind(&socket_path)?;
    let upload_id = uuid::Uuid::new_v4();
    let preflight = preflight_upload_target(&server, "/service.sock")?;
    assert!(preflight.exists);
    assert!(!preflight.replaceable);
    let response = with_upload_headers(
        server.request(
            reqwest::Method::PUT,
            format!("{}service.sock", server.url()),
        ),
        upload_id,
        1,
    )
    .timeout(Duration::from_secs(2))
    .body(b"x".to_vec())
    .send()?;

    assert_eq!(response.status(), 409);
    assert_eq!(
        response.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    assert_eq!(response.headers().get("x-dufs-upload-length").unwrap(), "1");
    assert_eq!(
        response.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    assert_problem_code(response, "destination_exists")?;
    assert!(
        std::fs::symlink_metadata(socket_path)?
            .file_type()
            .is_socket()
    );
    Ok(())
}

#[rstest]
fn put_overwrite_preserves_owner_mode_and_extended_attributes(
    server: TestServer,
) -> Result<(), Error> {
    use rustix::fs::{XattrFlags, fgetxattr, fsetxattr};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let target = server.path().join("metadata.txt");
    std::fs::write(&target, b"old")?;
    // Set the user xattr while the unprivileged fixture owner still has write
    // permission. Linux correctly refuses fsetxattr after mode becomes 0440.
    let original_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)?;
    fsetxattr(
        &original_file,
        "user.dufs-test",
        b"preserved",
        XattrFlags::empty(),
    )?;
    drop(original_file);
    // A read-only target exercises the final commit boundary: metadata replay
    // makes the hidden stage read-only, so no resumable checkpoint may be
    // created between replay and rename.
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o440))?;
    let original = std::fs::metadata(&target)?;
    let original_inode = original.ino();
    let preflight = preflight_upload_target(&server, "/metadata.txt")?;
    assert!(preflight.exists && preflight.replaceable);
    let revision = preflight
        .revision
        .ok_or("metadata target has no revision")?;

    let response = with_new_upload_overwrite_headers(
        server.request(
            reqwest::Method::PUT,
            format!("{}metadata.txt", server.url()),
        ),
        11,
        &revision,
    )
    .body(b"replacement".to_vec())
    .send()?;
    assert_eq!(response.status(), 201);

    let replaced = std::fs::metadata(&target)?;
    assert_ne!(replaced.ino(), original_inode);
    assert_eq!(replaced.uid(), original.uid());
    assert_eq!(replaced.gid(), original.gid());
    assert_eq!(replaced.permissions().mode() & 0o7777, 0o440);
    let replaced_file = std::fs::File::open(&target)?;
    let mut value = vec![0_u8; 64];
    let length = fgetxattr(&replaced_file, "user.dufs-test", &mut value)?;
    value.truncate(length);
    assert_eq!(value, b"preserved");
    assert_eq!(std::fs::read(&target)?, b"replacement");
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
    let upload_id = Uuid::new_v4();
    let preflight = preflight_upload_target(&server, "/dir1")?;
    assert!(preflight.exists);
    assert!(!preflight.replaceable);
    let resp = with_upload_headers(server.request(reqwest::Method::PUT, &url), upload_id, 3)
        .body(b"abc".to_vec())
        .send()?;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    assert_eq!(resp.headers().get("x-dufs-upload-length").unwrap(), "3");
    assert_eq!(
        resp.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    assert_problem_code(resp, "destination_exists")?;
    Ok(())
}
