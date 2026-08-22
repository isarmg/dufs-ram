use super::*;

#[rstest]
fn upload_preflight_rejects_ambiguous_or_oversized_requests(
    server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "")?;

    let empty = upload_preflight_raw(
        &context,
        Method::POST,
        Some("application/json"),
        br#"{"paths":[]}"#.to_vec(),
    )?;
    assert_api_problem(
        empty,
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "request_body_too_large",
    )?;

    let too_many_paths = (0..513)
        .map(|index| format!("/bulk/{index}.txt"))
        .collect::<Vec<_>>();
    let too_many = upload_preflight_raw(
        &context,
        Method::POST,
        Some("application/json"),
        serde_json::to_vec(&json!({ "paths": too_many_paths }))?,
    )?;
    assert_api_problem(
        too_many,
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "request_body_too_large",
    )?;

    let oversized_path = format!("/{}", "a".repeat(256 * 1024));
    let decoded_too_large = upload_preflight_raw(
        &context,
        Method::POST,
        Some("application/json"),
        serde_json::to_vec(&json!({ "paths": [oversized_path] }))?,
    )?;
    assert_api_problem(
        decoded_too_large,
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "request_body_too_large",
    )?;

    let duplicate = upload_preflight_raw(
        &context,
        Method::POST,
        Some("application/json"),
        br#"{"paths":["/same.txt","/same.txt"]}"#.to_vec(),
    )?;
    assert_api_problem(
        duplicate,
        reqwest::StatusCode::BAD_REQUEST,
        "invalid_request",
    )?;

    let invalid_path = upload_preflight_raw(
        &context,
        Method::POST,
        Some("application/json"),
        br#"{"paths":["relative.txt"]}"#.to_vec(),
    )?;
    assert_api_problem(
        invalid_path,
        reqwest::StatusCode::BAD_REQUEST,
        "invalid_path",
    )?;

    let wrong_content_type = upload_preflight_raw(
        &context,
        Method::POST,
        Some("text/plain"),
        br#"{"paths":["/valid.txt"]}"#.to_vec(),
    )?;
    assert_api_problem(
        wrong_content_type,
        reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    )?;

    let oversized_wire = upload_preflight_raw(
        &context,
        Method::POST,
        Some("application/json"),
        vec![b' '; 2 * 1024 * 1024 + 1],
    )?;
    assert_api_problem(
        oversized_wire,
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "request_body_too_large",
    )?;

    let wrong_method = upload_preflight_raw(&context, Method::GET, None, Vec::new())?;
    assert_eq!(wrong_method.headers().get("allow").unwrap(), "POST");
    assert_api_problem(
        wrong_method,
        reqwest::StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    )?;
    Ok(())
}

#[rstest]
fn upload_preflight_and_revision_condition_prevent_stale_overwrite(
    server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let target_path = server.path().join("preflight-existing.txt");
    std::fs::write(&target_path, b"original")?;

    let response = context
        .request(Method::POST, context.api_base.join("upload/preflight")?)
        .header("content-type", "application/json")
        .header(CSRF_HEADER, &context.csrf_token)
        .body(
            json!({
                "paths": ["/preflight-existing.txt", "/preflight-missing.txt"]
            })
            .to_string(),
        )
        .send()?;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("cache-control").unwrap(),
        "private, no-store"
    );
    let response = response_json(response)?;
    let targets = response["targets"]
        .as_array()
        .ok_or("preflight response is missing targets")?;
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0]["path"], "/preflight-existing.txt");
    assert_eq!(targets[0]["exists"], true);
    assert_eq!(targets[0]["replaceable"], true);
    let stale_revision = targets[0]["revision"]
        .as_str()
        .ok_or("existing target is missing its revision")?
        .to_string();
    assert!(!stale_revision.is_empty());
    assert_eq!(
        targets[1],
        json!({
            "path": "/preflight-missing.txt",
            "exists": false,
            "revision": null,
            "replaceable": true
        })
    );

    // The user confirmed the object observed above, but another writer changed
    // it before PUT admission. The stale revision must not authorize replacing
    // the newer object.
    std::fs::write(&target_path, b"competitor")?;
    let upload_id = Uuid::new_v4();
    let stale = with_upload_overwrite_headers(
        context
            .request(Method::PUT, server.url().join("preflight-existing.txt")?)
            .header(CSRF_HEADER, &context.csrf_token),
        upload_id,
        11,
        &stale_revision,
    )
    .body(b"replacement".to_vec())
    .send()?;
    assert_eq!(stale.status(), 409);
    assert_eq!(
        stale.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    assert_eq!(
        stale.headers().get("x-dufs-upload-id").unwrap(),
        upload_id.to_string().as_str()
    );
    let current_revision = stale
        .headers()
        .get("x-dufs-target-revision")
        .ok_or("destination conflict is missing its current revision")?
        .to_str()?
        .to_string();
    assert_ne!(current_revision, stale_revision);
    let problem = response_json(stale)?;
    assert_eq!(problem["code"], "destination_exists");
    assert_eq!(problem["recovery"], "refresh_target");
    assert_eq!(std::fs::read(&target_path)?, b"competitor");

    // Even a fresh upload ID cannot replace an existing object unless the
    // caller explicitly supplies overwrite consent bound to this revision.
    let default_no_replace_id = Uuid::new_v4();
    let no_replace = with_upload_headers(
        context
            .request(Method::PUT, server.url().join("preflight-existing.txt")?)
            .header(CSRF_HEADER, &context.csrf_token),
        default_no_replace_id,
        11,
    )
    .body(b"replacement".to_vec())
    .send()?;
    assert_eq!(no_replace.status(), 409);
    assert_eq!(
        no_replace.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    assert_eq!(
        no_replace.headers().get("x-dufs-target-revision").unwrap(),
        current_revision.as_str()
    );
    assert_eq!(response_json(no_replace)?["code"], "destination_exists");
    assert_eq!(std::fs::read(&target_path)?, b"competitor");

    let fresh = preflight_upload_target(&server, "/preflight-existing.txt")?;
    assert!(fresh.exists && fresh.replaceable);
    let fresh_revision = fresh
        .revision
        .ok_or("fresh preflight revision is missing")?;
    assert_eq!(fresh_revision, current_revision);
    let committed = with_upload_overwrite_headers(
        server.request(Method::PUT, server.url().join("preflight-existing.txt")?),
        Uuid::new_v4(),
        11,
        &fresh_revision,
    )
    .body(b"replacement".to_vec())
    .send()?;
    assert_eq!(committed.status(), 201);
    assert_eq!(
        committed.headers().get("x-dufs-operation-state").unwrap(),
        "committed"
    );
    assert_eq!(std::fs::read(&target_path)?, b"replacement");

    let replacement = preflight_upload_target(&server, "/preflight-existing.txt")?;
    let replacement_revision = replacement
        .revision
        .ok_or("replacement file has no revision")?;
    std::fs::remove_file(&target_path)?;
    let missing_transition_id = Uuid::new_v4();
    let missing = with_upload_overwrite_headers(
        server.request(Method::PUT, server.url().join("preflight-existing.txt")?),
        missing_transition_id,
        11,
        &replacement_revision,
    )
    .body(b"safe-create".to_vec())
    .send()?;
    assert_eq!(missing.status(), 409);
    assert_eq!(
        missing.headers().get("x-dufs-operation-state").unwrap(),
        "not-started"
    );
    assert_eq!(
        missing.headers().get("x-dufs-target-replaceable").unwrap(),
        "true"
    );
    assert!(!missing.headers().contains_key("x-dufs-target-revision"));
    assert_eq!(response_json(missing)?["code"], "upload_target_changed");
    assert!(!target_path.exists());

    let created = with_upload_headers(
        server.request(Method::PUT, server.url().join("preflight-existing.txt")?),
        missing_transition_id,
        11,
    )
    .header("X-Dufs-Upload-Overwrite", "false")
    .body(b"safe-create".to_vec())
    .send()?;
    assert_eq!(created.status(), 201);
    assert_eq!(std::fs::read(&target_path)?, b"safe-create");
    Ok(())
}

#[rstest]
fn list_items_expose_the_same_opaque_revision_as_upload_preflight(
    server: TestServer,
) -> Result<(), Error> {
    let response = server.list_api("/", &[("limit", "500")])?;
    assert_eq!(response.status(), 200);
    let listing = response_json(response)?;
    let item = listing["paths"]
        .as_array()
        .ok_or("list response omitted paths")?
        .iter()
        .find(|item| item["name"] == "test.html")
        .ok_or("list response omitted test.html")?;
    let revision = item["revision"]
        .as_str()
        .ok_or("list item omitted revision")?;
    assert_eq!(revision.len(), 64);
    assert!(
        revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(
        preflight_upload_target(&server, "/test.html")?
            .revision
            .as_deref(),
        Some(revision)
    );
    Ok(())
}
