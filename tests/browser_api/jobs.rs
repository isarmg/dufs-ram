use super::*;

#[rstest]
fn job_status_requires_authentication_validates_routing_and_delete_replays(
    server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let operation_id = Uuid::new_v4();
    let target_url = server.url().join("delete-operation.txt")?;
    std::fs::write(server.path().join("delete-operation.txt"), "delete me")?;
    let revision = preflight_upload_target(&server, "/delete-operation.txt")?
        .revision
        .ok_or("delete target has no revision")?;

    let delete = || {
        context
            .request(Method::DELETE, target_url.clone())
            .header(CSRF_HEADER, &context.csrf_token)
            .header("X-Dufs-Operation-Id", operation_id.to_string())
            .header("If-Match", format!("\"{revision}\""))
            .send()
    };
    let first = delete()?;
    assert_eq!(first.status(), 204);
    let replay = delete()?;
    assert_eq!(replay.status(), 204);
    assert_eq!(
        replay.headers().get("x-dufs-operation-replayed").unwrap(),
        "true"
    );
    assert!(!server.path().join("delete-operation.txt").exists());

    let status_url = context.api_base.join(&format!("jobs/{operation_id}"))?;
    let unauthenticated = server.raw_request(Method::GET, status_url.clone()).send()?;
    assert_eq!(unauthenticated.status(), 401);
    assert_eq!(
        unauthenticated.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        response_json(unauthenticated)?["code"],
        "auth.session_required"
    );
    let authenticated = context.request(Method::GET, status_url).send()?;
    assert_eq!(authenticated.status(), 200);
    assert_eq!(response_json(authenticated)?["state"], "succeeded");

    let invalid = context
        .request(Method::GET, context.api_base.join("jobs/not-a-uuid")?)
        .send()?;
    assert_eq!(invalid.status(), 400);
    assert_eq!(response_json(invalid)?["code"], "invalid_job_id");

    let unknown = context
        .request(
            Method::GET,
            context.api_base.join(&format!("jobs/{}", Uuid::new_v4()))?,
        )
        .send()?;
    assert_eq!(unknown.status(), 404);
    assert_eq!(response_json(unknown)?["code"], "job_not_found");

    let wrong_method = context
        .request(
            Method::POST,
            context.api_base.join(&format!("jobs/{operation_id}"))?,
        )
        .header(CSRF_HEADER, &context.csrf_token)
        .send()?;
    assert_eq!(wrong_method.status(), 405);
    assert_eq!(wrong_method.headers().get("allow").unwrap(), "GET");
    assert_eq!(response_json(wrong_method)?["code"], "method_not_allowed");

    let root_operation_id = Uuid::new_v4();
    let root_delete = context
        .request(Method::DELETE, server.url().clone())
        .header(CSRF_HEADER, &context.csrf_token)
        .header("X-Dufs-Operation-Id", root_operation_id.to_string())
        .send()?;
    assert_eq!(root_delete.status(), 403);
    assert_eq!(
        root_delete.headers().get("x-dufs-operation-id").unwrap(),
        root_operation_id.to_string().as_str()
    );
    assert_eq!(
        root_delete.headers().get("x-dufs-operation-state").unwrap(),
        "rejected"
    );
    let root_delete = response_json(root_delete)?;
    assert_eq!(root_delete["code"], "root_delete_forbidden");
    assert_eq!(root_delete["state"], "rejected");
    Ok(())
}

#[rstest]
fn delete_if_match_rejects_a_source_replaced_by_another_client(
    server: TestServer,
) -> Result<(), Error> {
    let target = server.path().join("delete-race.txt");
    std::fs::write(&target, "client-a-saw-this")?;
    let stale_revision = preflight_upload_target(&server, "/delete-race.txt")?
        .revision
        .ok_or("delete race target has no revision")?;

    let client_b = with_upload_overwrite_headers(
        server.request(Method::PUT, server.url().join("delete-race.txt")?),
        Uuid::new_v4(),
        20,
        &stale_revision,
    )
    .body(b"client-b-replacement".to_vec())
    .send()?;
    assert_eq!(client_b.status(), 201);

    let context = browser_context(&server, "")?;
    let stale_delete = context
        .request(Method::DELETE, server.url().join("delete-race.txt")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .header("If-Match", format!("\"{stale_revision}\""))
        .send()?;
    assert_eq!(stale_delete.status(), 412);
    assert_eq!(
        stale_delete
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "failed"
    );
    let current_revision = stale_delete
        .headers()
        .get("x-dufs-source-revision")
        .ok_or("stale delete response omitted its current revision")?
        .to_str()?
        .to_owned();
    assert_ne!(current_revision, stale_revision);
    assert_eq!(
        response_json(stale_delete)?["code"],
        "delete_target_changed"
    );
    assert_eq!(std::fs::read_to_string(target)?, "client-b-replacement");
    Ok(())
}

#[rstest]
fn delete_requires_one_strong_quoted_revision(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let target = server.path().join("conditional-delete.txt");
    std::fs::write(&target, "keep")?;
    let revision = preflight_upload_target(&server, "/conditional-delete.txt")?
        .revision
        .ok_or("conditional delete target has no revision")?;

    let missing = context
        .request(Method::DELETE, server.url().join("conditional-delete.txt")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .send()?;
    assert_eq!(missing.status(), 428);
    assert_eq!(response_json(missing)?["code"], "source_revision_required");
    assert!(target.is_file());

    let unquoted = context
        .request(Method::DELETE, server.url().join("conditional-delete.txt")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .header("If-Match", revision)
        .send()?;
    assert_eq!(unquoted.status(), 400);
    assert_eq!(response_json(unquoted)?["code"], "invalid_source_revision");
    assert!(target.is_file());
    Ok(())
}

#[rstest]
fn malformed_operation_id_is_rejected_before_mutation(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "application/json")
        .header("X-Dufs-Operation-Id", "NOT-A-CANONICAL-UUID")
        .body(json!({"path": "/must-not-exist"}).to_string())
        .send()?;
    assert_eq!(response.status(), 400);
    assert_eq!(response_json(response)?["code"], "invalid_operation_id");
    assert!(!server.path().join("must-not-exist").exists());
    Ok(())
}

#[rstest]
fn browser_api_requires_operation_id_before_mutation(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "application/json")
        .body(json!({"path": "/must-not-exist"}).to_string())
        .send()?;
    assert_eq!(response.status(), 400);
    let body = response_json(response)?;
    assert_eq!(body["code"], "invalid_operation_id");
    assert!(body.get("operation_id").is_none());
    assert!(body.get("state").is_none());
    assert_eq!(body["detail"], "The x-dufs-operation-id header is required");
    assert!(body.get("message").is_none());
    assert!(!server.path().join("must-not-exist").exists());

    let delete_target = server.path().join("must-not-delete.txt");
    std::fs::write(&delete_target, "keep")?;
    let response = context
        .request(Method::DELETE, server.url().join("must-not-delete.txt")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .send()?;
    assert_eq!(response.status(), 400);
    let body = response_json(response)?;
    assert_eq!(body["code"], "invalid_operation_id");
    assert!(body.get("operation_id").is_none());
    assert!(body.get("state").is_none());
    assert!(delete_target.is_file());
    Ok(())
}

#[rstest]
fn operation_header_is_ignored_for_non_mutation_requests(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = context
        .request(Method::GET, server.url())
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .send()?;
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("x-dufs-operation-id"));
    assert!(!response.headers().contains_key("x-dufs-operation-state"));
    Ok(())
}
