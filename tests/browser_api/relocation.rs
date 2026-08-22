use super::*;

#[rstest]
fn move_file_to_directory_and_preserve_its_name(server: TestServer) -> Result<(), Error> {
    std::fs::create_dir(server.path().join("moved"))?;
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "move",
        json!({
            "source": "/test.html",
            "directory": "/moved",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 204);
    assert!(!server.path().join("test.html").exists());
    assert_eq!(
        std::fs::read_to_string(server.path().join("moved/test.html"))?,
        "This is test.html"
    );
    Ok(())
}

#[rstest]
fn move_accepts_root_and_requires_an_existing_directory(server: TestServer) -> Result<(), Error> {
    std::fs::write(server.path().join("dir1/to-root.txt"), "root move")?;
    let context = browser_context(&server, "")?;
    let to_root = post_json(
        &context,
        "move",
        json!({
            "source": "/dir1/to-root.txt",
            "directory": "/",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(to_root.status(), 204);
    assert_eq!(
        std::fs::read_to_string(server.path().join("to-root.txt"))?,
        "root move"
    );

    let missing_directory = post_json(
        &context,
        "move",
        json!({
            "source": "/test.html",
            "directory": "/missing-directory",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(missing_directory.status(), 404);
    assert_eq!(
        response_json(missing_directory)?["code"],
        "destination_directory_not_found"
    );

    let not_directory = post_json(
        &context,
        "move",
        json!({
            "source": "/test.html",
            "directory": "/index.html",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(not_directory.status(), 409);
    assert_eq!(
        response_json(not_directory)?["code"],
        "destination_not_directory"
    );
    assert!(server.path().join("test.html").is_file());
    Ok(())
}

#[rstest]
fn rename_directory_within_its_parent(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "rename",
        json!({
            "source": "/dir1",
            "name": "renamed-dir",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 204);
    assert!(!server.path().join("dir1").exists());
    assert!(server.path().join("renamed-dir/test.html").is_file());
    Ok(())
}

#[rstest]
fn rename_rejects_invalid_names_and_legacy_destination_payloads(
    server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    for name in ["", ".", "..", "nested/name", "__dufs__"] {
        let response = post_json(
            &context,
            "rename",
            json!({
                "source": "/test.html",
                "name": name,
                "overwrite": false
            }),
            Some(&context.csrf_token),
        )?;
        assert_eq!(response.status(), 400, "name={name:?}");
        assert_eq!(
            response_json(response)?["code"],
            "invalid_rename_name",
            "name={name:?}"
        );
    }

    let legacy = post_json(
        &context,
        "move",
        json!({
            "source": "/test.html",
            "destination": "/dir1/test.html",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(legacy.status(), 400);
    assert_eq!(response_json(legacy)?["code"], "invalid_json");
    assert!(server.path().join("test.html").is_file());
    Ok(())
}

#[rstest]
fn durable_state_blocks_namespace_rebases_through_symlink_aliases(
    server: TestServer,
) -> Result<(), Error> {
    let real = server.path().join("state-target");
    let alias = server.path().join("state-alias");
    std::fs::create_dir(&real)?;
    symlink("state-target", &alias)?;

    let upload_id = Uuid::new_v4();
    let partial = with_upload_headers(
        server.request(Method::PUT, server.url().join("state-alias/pending.bin")?),
        upload_id,
        6,
    )
    .body(b"abc".to_vec())
    .send()?;
    assert_eq!(partial.status(), 409);
    assert_eq!(
        partial.headers().get("x-dufs-operation-state").unwrap(),
        "running"
    );

    let same_target_upload_id = Uuid::new_v4();
    let same_target = with_upload_headers(
        server.request(Method::PUT, server.url().join("state-alias/pending.bin")?),
        same_target_upload_id,
        3,
    )
    .body(b"new".to_vec())
    .send()?;
    assert_eq!(same_target.status(), 201);
    assert_eq!(std::fs::read(real.join("pending.bin"))?, b"new");

    let context = browser_context(&server, "")?;
    let source_rename_id = Uuid::new_v4();
    let source_request = json!({
        "source": "/state-target",
        "name": "renamed-state-target",
        "overwrite": false
    });
    let source_rebase =
        post_json_with_operation(&context, "rename", source_request.clone(), source_rename_id)?;
    assert_eq!(source_rebase.status(), 409);
    assert_eq!(
        source_rebase
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "failed"
    );
    assert_eq!(
        response_json(source_rebase)?["code"],
        "rename_state_conflict"
    );
    let replay = post_json_with_operation(&context, "rename", source_request, source_rename_id)?;
    assert_eq!(replay.status(), 409);
    assert_eq!(
        replay.headers().get("x-dufs-operation-replayed").unwrap(),
        "true"
    );
    assert_eq!(response_json(replay)?["code"], "rename_state_conflict");

    std::fs::create_dir(server.path().join("move-target"))?;
    let move_rebase = post_json(
        &context,
        "move",
        json!({
            "source": "/state-target",
            "directory": "/move-target",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(move_rebase.status(), 409);
    assert_eq!(response_json(move_rebase)?["code"], "move_state_conflict");
    assert!(server.path().join("state-target").is_dir());

    let replacement_upload_id = Uuid::new_v4();
    let upload_rebase = with_upload_headers(
        server.request(Method::PUT, server.url().join("state-alias")?),
        replacement_upload_id,
        3,
    )
    .body(b"new".to_vec())
    .send()?;
    assert_eq!(upload_rebase.status(), 409);
    assert_eq!(
        upload_rebase
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "not-started"
    );
    let upload_rebase = response_json(upload_rebase)?;
    assert_eq!(upload_rebase["code"], "upload_state_conflict");
    assert_eq!(upload_rebase["upload_state"], "not-started");
    assert_eq!(upload_rebase["recovery"], "refresh_target");

    let delete_operation_id = Uuid::new_v4();
    let delete_rebase = context
        .request(Method::DELETE, server.url().join("state-alias")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("X-Dufs-Operation-Id", delete_operation_id.to_string())
        .send()?;
    assert_eq!(delete_rebase.status(), 409);
    assert_eq!(
        delete_rebase
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "failed"
    );
    assert_eq!(
        response_json(delete_rebase)?["code"],
        "delete_state_conflict"
    );

    assert!(real.is_dir());
    assert!(std::fs::symlink_metadata(&alias)?.file_type().is_symlink());
    assert_eq!(std::fs::read(real.join("pending.bin"))?, b"new");
    Ok(())
}

#[rstest]
fn rename_requires_explicit_overwrite_and_no_replace_has_one_winner(
    server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let request = json!({
        "source": "/test.html",
        "name": "index.html",
        "overwrite": false
    });
    let response = post_json(
        &context,
        "rename",
        request.clone(),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 409);
    assert!(server.path().join("test.html").is_file());
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );

    let mut overwrite = request;
    overwrite["overwrite"] = Value::Bool(true);
    let response = post_json(&context, "rename", overwrite, Some(&context.csrf_token))?;
    assert_eq!(response.status(), 204);
    assert!(!server.path().join("test.html").exists());
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is test.html"
    );
    assert_concurrent_no_replace_renames(&context, &server)?;
    Ok(())
}

#[rstest]
fn rename_overwrite_rejects_hardlink_aliases_without_claiming_success(
    server: TestServer,
) -> Result<(), Error> {
    std::fs::write(server.path().join("hardlink-source.txt"), "shared")?;
    std::fs::hard_link(
        server.path().join("hardlink-source.txt"),
        server.path().join("hardlink-destination.txt"),
    )?;
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "rename",
        json!({
            "source": "/hardlink-source.txt",
            "name": "hardlink-destination.txt",
            "overwrite": true
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 409);
    assert_eq!(
        response_json(response)?["code"],
        "source_equals_destination"
    );
    assert!(server.path().join("hardlink-source.txt").exists());
    assert!(server.path().join("hardlink-destination.txt").exists());
    Ok(())
}

#[rstest]
fn move_rejects_missing_source_and_directory_overwrite(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let missing = post_json(
        &context,
        "move",
        json!({
            "source": "/missing",
            "directory": "/dir1",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(missing.status(), 404);

    std::fs::create_dir(server.path().join("target-parent"))?;
    std::fs::create_dir(server.path().join("target-parent/dir1"))?;
    let directory_overwrite = post_json(
        &context,
        "move",
        json!({
            "source": "/dir1",
            "directory": "/target-parent",
            "overwrite": true
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(directory_overwrite.status(), 409);
    assert!(server.path().join("dir1").is_dir());
    assert!(server.path().join("target-parent/dir1").is_dir());
    Ok(())
}

#[rstest]
fn browser_api_rejects_invalid_logical_paths(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    for path in ["/", "/../escape", "/a//b", "/__dufs__/reserved"] {
        let response = post_json(
            &context,
            "mkdir",
            json!({"path": path}),
            Some(&context.csrf_token),
        )?;
        assert_eq!(response.status(), 400, "path={path}");
    }

    let same_path = post_json(
        &context,
        "move",
        json!({"source": "/dir1", "directory": "/"}),
        Some(&context.csrf_token),
    )?;
    assert_eq!(same_path.status(), 400);

    let descendant = post_json(
        &context,
        "move",
        json!({
            "source": "/dir1",
            "directory": "/dir1",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(descendant.status(), 409);
    Ok(())
}

#[rstest]
fn rename_names_do_not_percent_decode(server: TestServer) -> Result<(), Error> {
    std::fs::write(server.path().join("literal%2F.txt"), "literal percent")?;
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "rename",
        json!({
            "source": "/literal%2F.txt",
            "name": "moved%2F.txt",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 204);
    assert_eq!(
        std::fs::read_to_string(server.path().join("moved%2F.txt"))?,
        "literal percent"
    );
    assert!(!server.path().join("moved/2F.txt").exists());
    Ok(())
}
