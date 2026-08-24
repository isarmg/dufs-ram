use super::*;

#[test]
fn sqlite_state_dir_resumes_upload_after_restart() -> Result<(), Error> {
    let state_dir = assert_fs::TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let state_db = state_dir.path().join("state.sqlite3");
    let state_args = [
        OsString::from("--state-dir"),
        state_dir.path().as_os_str().to_owned(),
    ];
    let mut server = server(state_args.clone(), &[TEST_ACCOUNT]);
    let upload_id = Uuid::new_v4();
    let target_name = "sqlite-resumable.bin";
    let upload_length = 6_u64;
    let target_url = server.url().join(target_name)?;

    let first = with_upload_headers(
        server.request(Method::PUT, target_url),
        upload_id,
        upload_length,
    )
    .body(b"abc".to_vec())
    .send()?;
    assert_eq!(first.status(), 409);
    assert_eq!(
        first.headers().get("x-dufs-operation-state").unwrap(),
        "running"
    );
    assert_eq!(first.headers().get("x-dufs-upload-offset").unwrap(), "3");

    let stage_path = std::fs::read_dir(server.path().join(UPLOAD_STAGE_DIRECTORY))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".dufs-upload-") && name.ends_with(".part")
            })
        })
        .ok_or("SQLite-backed upload did not create a staging file")?;
    let stage_name = stage_path
        .file_name()
        .ok_or("upload stage has no file name")?;
    let expected_stage_path = std::path::Path::new(UPLOAD_STAGE_DIRECTORY).join(stage_name);
    assert!(stage_path.is_file());

    let checkpoint = sqlite_upload_checkpoint(&state_db, upload_id)?;
    assert_eq!(checkpoint.0, target_name.as_bytes());
    assert_eq!(
        checkpoint.1,
        expected_stage_path.as_os_str().as_encoded_bytes()
    );
    assert_eq!(checkpoint.2, upload_length as i64);
    assert_eq!(checkpoint.3, 3);
    assert_eq!(checkpoint.4, 0, "checkpoint must be in the running state");

    server.restart_with_default_auth_args(state_args.clone());
    let target_url = server.url().join(target_name)?;
    let status = server
        .request(Method::HEAD, target_url.clone())
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(status.status(), 200);
    assert_eq!(
        status.headers().get("x-dufs-operation-state").unwrap(),
        "running"
    );
    assert_eq!(status.headers().get("x-dufs-upload-length").unwrap(), "6");
    assert_eq!(status.headers().get("x-dufs-upload-offset").unwrap(), "3");

    let completed = with_resume_upload_headers(
        server.request(Method::PATCH, target_url.clone()),
        upload_id,
        upload_length,
        3,
    )
    .body(b"123".to_vec())
    .send()?;
    assert_eq!(completed.status(), 204);
    assert_eq!(std::fs::read(server.path().join(target_name))?, b"abc123");
    assert!(!stage_path.exists());

    let committed = sqlite_upload_checkpoint(&state_db, upload_id)?;
    assert_eq!(committed.0, target_name.as_bytes());
    assert_eq!(
        committed.1,
        expected_stage_path.as_os_str().as_encoded_bytes()
    );
    assert_eq!(committed.2, upload_length as i64);
    assert_eq!(committed.3, upload_length as i64);
    assert_eq!(committed.4, 2, "completed upload must be committed");

    server.restart_with_default_auth_args(state_args);
    let status = server
        .request(Method::HEAD, server.url().join(target_name)?)
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .send()?;
    assert_eq!(status.status(), 200);
    assert_eq!(
        status.headers().get("x-dufs-operation-state").unwrap(),
        "committed"
    );
    assert_eq!(status.headers().get("x-dufs-upload-offset").unwrap(), "6");
    Ok(())
}

#[test]
fn sqlite_delete_outbox_purges_trash_and_replays_after_restart() -> Result<(), Error> {
    let state_dir = assert_fs::TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let state_db = state_dir.path().join("state.sqlite3");
    let state_args = [
        OsString::from("--state-dir"),
        state_dir.path().as_os_str().to_owned(),
    ];
    let mut server = server(state_args.clone(), &[TEST_ACCOUNT]);
    let target = server.path().join("durable-delete");
    std::fs::create_dir(&target)?;
    for index in 0..1024 {
        std::fs::write(target.join(format!("{index:04}.txt")), b"delete")?;
    }
    let operation_id = Uuid::new_v4();
    let revision = preflight_upload_target(&server, "/durable-delete")?
        .revision
        .ok_or("durable delete target has no revision")?;

    {
        let context = browser_context(&server, "")?;
        let response = context
            .request(Method::DELETE, server.url().join("durable-delete/")?)
            .header(CSRF_HEADER, &context.csrf_token)
            .header("X-Dufs-Operation-Id", operation_id.to_string())
            .header("If-Match", format!("\"{revision}\""))
            .send()?;
        assert_eq!(response.status(), 204);
        assert_eq!(
            response.headers().get("x-dufs-operation-state").unwrap(),
            "succeeded"
        );
        assert!(!target.exists(), "DELETE must atomically hide the target");

        let status = context
            .request(
                Method::GET,
                context.api_base.join(&format!("jobs/{operation_id}"))?,
            )
            .send()?;
        assert_eq!(status.status(), 200);
        assert_eq!(response_json(status)?["state"], "succeeded");
    }

    let connection = Connection::open(&state_db)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pending: i64 =
            connection.query_row("SELECT COUNT(*) FROM purge_jobs", [], |row| row.get(0))?;
        if pending == 0 && !has_internal_delete_trash(server.path())? {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "durable delete outbox did not drain: pending_jobs={pending}, trash_present={}",
                has_internal_delete_trash(server.path())?
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(connection);

    server.restart_with_default_auth_args(state_args);
    let context = browser_context(&server, "")?;
    let replay = context
        .request(Method::DELETE, server.url().join("durable-delete/")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("X-Dufs-Operation-Id", operation_id.to_string())
        .header("If-Match", format!("\"{revision}\""))
        .send()?;
    assert_eq!(replay.status(), 204);
    assert_eq!(
        replay.headers().get("x-dufs-operation-state").unwrap(),
        "succeeded"
    );
    assert_eq!(
        replay.headers().get("x-dufs-operation-replayed").unwrap(),
        "true"
    );
    assert!(!target.exists());
    assert!(!has_internal_delete_trash(server.path())?);
    let status = context
        .request(
            Method::GET,
            context.api_base.join(&format!("jobs/{operation_id}"))?,
        )
        .send()?;
    assert_eq!(status.status(), 200);
    assert_eq!(response_json(status)?["state"], "succeeded");
    Ok(())
}

#[test]
fn sqlite_claimed_delete_job_is_recovered_and_purged_after_forced_restart() -> Result<(), Error> {
    let state_dir = assert_fs::TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let state_db = state_dir.path().join("state.sqlite3");
    let state_args = [
        OsString::from("--state-dir"),
        state_dir.path().as_os_str().to_owned(),
    ];
    let mut server = server(state_args.clone(), &[TEST_ACCOUNT]);
    let job_id = Uuid::new_v4();
    let trash_name = format!(".dufs-upload-delete-{job_id}.trash");
    let trash = server.path().join(&trash_name);
    std::fs::create_dir(&trash)?;
    std::fs::write(trash.join("payload.bin"), b"pending purge")?;
    let metadata = std::fs::symlink_metadata(&trash)?;
    let owner_digest = [0x42_u8; 32];
    let source_device = metadata.dev().to_be_bytes();
    let source_inode = metadata.ino().to_be_bytes();

    let connection = Connection::open(&state_db)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    connection.execute(
        "INSERT INTO purge_jobs(
             owner_digest, job_id, target_path, trash_path,
             source_device_be, source_inode_be, is_directory, state, attempts,
             next_attempt_at_ms, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 2, 0, 0, 0, 0)",
        params![
            owner_digest.as_slice(),
            job_id.as_bytes().as_slice(),
            b"forced-restart-source".as_slice(),
            trash_name.as_bytes(),
            source_device.as_slice(),
            source_inode.as_slice(),
        ],
    )?;
    let claimed: i64 = connection.query_row(
        "SELECT state FROM purge_jobs WHERE job_id = ?1",
        params![job_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    assert_eq!(claimed, 2, "fixture must remain claimed before restart");
    assert!(trash.is_dir(), "fixture trash disappeared before restart");
    drop(connection);

    // The fixture restart uses Child::kill, so startup recovery—not graceful
    // shutdown—is responsible for returning Claimed to Ready and purging it.
    server.restart_with_default_auth_args(state_args);

    let connection = Connection::open(&state_db)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pending: i64 = connection.query_row(
            "SELECT COUNT(*) FROM purge_jobs WHERE job_id = ?1",
            params![job_id.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if pending == 0 && !trash.exists() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "claimed purge job did not recover after restart: pending_jobs={pending}, trash_present={}",
                trash.exists()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !trash.exists(),
        "restart recovery left durable trash behind"
    );
    Ok(())
}
