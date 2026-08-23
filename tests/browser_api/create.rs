use super::*;

#[rstest]
fn mkdir_creates_nested_unicode_directory(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "mkdir",
        json!({"path": "/新目录/nested"}),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 201);
    assert!(server.path().join("新目录/nested").is_dir());
    Ok(())
}

#[rstest]
fn noncanonical_internal_api_paths_are_rejected_before_operation_tracking(
    server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    for (index, raw_path) in [
        "/__dufs__/api/mkdir/",
        "//__dufs__/api/mkdir",
        "/%5F%5Fdufs%5F%5F/api/mkdir",
        "/__dufs__%2Fapi/mkdir",
    ]
    .into_iter()
    .enumerate()
    {
        let url = Url::parse(&format!("http://localhost:{}{raw_path}", server.port()))?;
        assert_eq!(url.path(), raw_path);
        let target = format!("/noncanonical-{index}");
        let response = context
            .request(Method::POST, url)
            .header("content-type", "application/json")
            .header(CSRF_HEADER, &context.csrf_token)
            .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
            .body(json!({"path": target}).to_string())
            .send()?;
        assert_eq!(response.status(), 400, "path={raw_path}");
        assert!(
            !response.headers().contains_key("x-dufs-operation-id"),
            "path={raw_path}"
        );
        assert!(!server.path().join(format!("noncanonical-{index}")).exists());
    }
    Ok(())
}

#[rstest]
fn mkdir_existing_path_conflicts(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "mkdir",
        json!({"path": "/dir1"}),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 409);
    let body = response_json(response)?;
    assert_eq!(body["code"], "path_exists");
    assert_eq!(body["detail"], "Path already exists");
    assert!(body.get("message").is_none());
    assert_eq!(body["state"], "failed");
    Ok(())
}

#[rstest]
fn mkdir_does_not_replace_or_descend_through_a_resumable_upload_path(
    server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "")?;

    for (target_name, directory_path) in [
        ("mkdir-upload-exact.bin", "/mkdir-upload-exact.bin"),
        (
            "mkdir-upload-ancestor.bin",
            "/mkdir-upload-ancestor.bin/child",
        ),
    ] {
        let upload_id = Uuid::new_v4();
        let target_url = server.url().join(target_name)?;
        let initial = with_upload_headers(
            server.request(Method::PUT, target_url.clone()),
            upload_id,
            6,
        )
        .body(b"abc".to_vec())
        .send()?;
        assert_eq!(initial.status(), 409);

        let mkdir = post_json(
            &context,
            "mkdir",
            json!({"path": directory_path}),
            Some(&context.csrf_token),
        )?;
        assert_eq!(mkdir.status(), 409);
        let mkdir = response_json(mkdir)?;
        assert_eq!(mkdir["code"], "mkdir_state_conflict");
        assert_eq!(mkdir["state"], "failed");
        assert!(!server.path().join(target_name).exists());

        let completed =
            with_resume_upload_headers(server.request(Method::PATCH, target_url), upload_id, 6, 3)
                .body(b"123".to_vec())
                .send()?;
        assert_eq!(completed.status(), 204);
        assert_eq!(std::fs::read(server.path().join(target_name))?, b"abc123");
    }
    Ok(())
}

#[rstest]
fn operation_id_makes_mkdir_idempotent_and_queryable(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let operation_id = Uuid::new_v4();
    let operation_id_string = operation_id.to_string();
    let request = json!({"path": "/idempotent-directory"});

    let first = post_json_with_operation(&context, "mkdir", request.clone(), operation_id)?;
    assert_eq!(first.status(), 201);
    assert_eq!(
        first.headers().get("x-dufs-operation-id").unwrap(),
        operation_id_string.as_str()
    );
    assert_eq!(
        first.headers().get("x-dufs-operation-state").unwrap(),
        "succeeded"
    );

    let replay = post_json_with_operation(&context, "mkdir", request, operation_id)?;
    assert_eq!(replay.status(), 201);
    assert_eq!(
        replay.headers().get("x-dufs-operation-replayed").unwrap(),
        "true"
    );
    assert!(server.path().join("idempotent-directory").is_dir());

    let conflict = post_json_with_operation(
        &context,
        "mkdir",
        json!({"path": "/different-directory"}),
        operation_id,
    )?;
    assert_eq!(conflict.status(), 409);
    assert_eq!(
        conflict.headers().get("x-dufs-operation-state").unwrap(),
        "rejected"
    );
    let conflict = response_json(conflict)?;
    assert_eq!(conflict["state"], "rejected");
    assert_eq!(conflict["code"], "operation_id_conflict");
    assert!(!server.path().join("different-directory").exists());

    let job_status = context
        .request(
            Method::GET,
            context.api_base.join(&format!("jobs/{operation_id}"))?,
        )
        .send()?;
    assert_eq!(job_status.status(), 200);
    assert_eq!(
        job_status.headers().get("content-type").unwrap(),
        "application/json"
    );
    let job_status = response_json(job_status)?;
    assert_eq!(
        job_status,
        json!({
            "job_id": operation_id_string,
            "state": "succeeded",
            "http_status": 201
        })
    );

    let removed_operation_status = context
        .request(
            Method::GET,
            context
                .api_base
                .join(&format!("operations/{operation_id}"))?,
        )
        .send()?;
    assert_eq!(removed_operation_status.status(), 404);
    assert_eq!(
        response_json(removed_operation_status)?["code"],
        "api_endpoint_not_found"
    );
    Ok(())
}

#[test]
fn sqlite_state_dir_replays_completed_mkdir_after_restart() -> Result<(), Error> {
    let state_dir = assert_fs::TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let state_db = state_dir.path().join("state.sqlite3");
    let state_args = [
        OsString::from("--state-dir"),
        state_dir.path().as_os_str().to_owned(),
    ];
    let mut server = server(state_args.clone());
    let operation_id = Uuid::new_v4();
    let request = json!({"path": "/persistent-idempotent-directory"});

    let context = browser_context(&server, "")?;
    let first = post_json_with_operation(&context, "mkdir", request.clone(), operation_id)?;
    assert_eq!(first.status(), 201);
    assert_eq!(
        first.headers().get("x-dufs-operation-state").unwrap(),
        "succeeded"
    );

    server.restart_with_default_auth_args(state_args);
    let context = browser_context(&server, "")?;
    let replay = post_json_with_operation(&context, "mkdir", request, operation_id)?;
    assert_eq!(replay.status(), 201);
    assert_eq!(
        replay.headers().get("x-dufs-operation-replayed").unwrap(),
        "true"
    );
    assert!(
        server
            .path()
            .join("persistent-idempotent-directory")
            .is_dir()
    );
    assert!(state_db.is_file(), "fixed SQLite database was not created");
    Ok(())
}
