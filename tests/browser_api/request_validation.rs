use super::*;

#[rstest]
fn browser_api_requires_csrf_and_json(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let unknown_endpoint = context
        .request(Method::POST, context.api_base.join("unknown")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "application/json")
        .body("{}")
        .send()?;
    assert_eq!(unknown_endpoint.status(), 404);
    assert_eq!(
        unknown_endpoint.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let unknown_endpoint = response_json(unknown_endpoint)?;
    assert_eq!(unknown_endpoint["status"], 404);
    assert_eq!(unknown_endpoint["code"], "api_endpoint_not_found");

    let wrong_method = context
        .request(Method::GET, context.api_base.join("mkdir")?)
        .send()?;
    assert_eq!(wrong_method.status(), 405);
    assert_eq!(wrong_method.headers().get("allow").unwrap(), "POST");
    assert_eq!(
        wrong_method.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    assert_eq!(response_json(wrong_method)?["code"], "method_not_allowed");

    let wrong_list_method = context
        .request(Method::POST, context.api_base.join("list")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .send()?;
    assert_eq!(wrong_list_method.status(), 405);
    assert_eq!(wrong_list_method.headers().get("allow").unwrap(), "GET");
    assert_eq!(
        response_json(wrong_list_method)?["code"],
        "method_not_allowed"
    );

    let missing_csrf = post_json(&context, "mkdir", json!({"path": "/blocked"}), None)?;
    assert_eq!(missing_csrf.status(), 403);
    assert_eq!(response_json(missing_csrf)?["code"], "auth.csrf_rejected");
    assert!(!server.path().join("blocked").exists());

    let invalid_csrf = post_json(
        &context,
        "mkdir",
        json!({"path": "/blocked"}),
        Some("invalid"),
    )?;
    assert_eq!(invalid_csrf.status(), 403);
    assert_eq!(response_json(invalid_csrf)?["code"], "auth.csrf_rejected");

    let wrong_type_operation_id = Uuid::new_v4();
    let wrong_type = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "text/plain")
        .header("X-Dufs-Operation-Id", wrong_type_operation_id.to_string())
        .body(r#"{"path":"/blocked"}"#)
        .send()?;
    assert_eq!(wrong_type.status(), 415);
    assert_eq!(
        wrong_type.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    assert_eq!(
        wrong_type.headers().get("x-dufs-operation-id").unwrap(),
        wrong_type_operation_id.to_string().as_str()
    );
    assert_eq!(
        wrong_type.headers().get("x-dufs-operation-state").unwrap(),
        "rejected"
    );
    let wrong_type = response_json(wrong_type)?;
    assert_eq!(
        wrong_type["type"],
        "urn:dufs:problem:unsupported_media_type"
    );
    assert_eq!(wrong_type["title"], "Unsupported Media Type");
    assert_eq!(wrong_type["status"], 415);
    assert_eq!(wrong_type["code"], "unsupported_media_type");
    assert_eq!(
        wrong_type["detail"],
        "Content-Type must be application/json"
    );
    assert!(wrong_type.get("message").is_none());
    assert_eq!(
        wrong_type["operation_id"],
        wrong_type_operation_id.to_string()
    );
    assert_eq!(wrong_type["state"], "rejected");

    let malformed_operation_id = Uuid::new_v4();
    let malformed = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "application/json")
        .header("X-Dufs-Operation-Id", malformed_operation_id.to_string())
        .body("{")
        .send()?;
    assert_eq!(malformed.status(), 400);
    let malformed = response_json(malformed)?;
    assert_eq!(malformed["code"], "invalid_json");
    assert_eq!(malformed["detail"], "Invalid JSON request");
    assert!(malformed.get("message").is_none());
    assert_eq!(malformed["state"], "failed");
    Ok(())
}

#[rstest]
fn file_mutations_require_session_csrf(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let file_url = server.url().join("csrf-protected.txt")?;
    let upload_id = Uuid::new_v4();
    let initial = "created";
    let remainder = " and appended";
    let upload_length = (initial.len() + remainder.len()) as u64;

    for csrf_token in [None, Some("invalid")] {
        let mut request = with_upload_headers(
            context.request(Method::PUT, file_url.clone()),
            upload_id,
            upload_length,
        )
        .body("blocked put");
        if let Some(csrf_token) = csrf_token {
            request = request.header(CSRF_HEADER, csrf_token);
        }
        let response = request.send()?;
        assert_eq!(response.status(), 403, "PUT csrf={csrf_token:?}");
        assert_eq!(response_json(response)?["code"], "auth.csrf_rejected");
        assert!(!server.path().join("csrf-protected.txt").exists());
    }

    let response = with_upload_headers(
        context.request(Method::PUT, file_url.clone()),
        upload_id,
        upload_length,
    )
    .header(CSRF_HEADER, &context.csrf_token)
    .body(initial)
    .send()?;
    assert_eq!(response.status(), 409);
    assert!(!server.path().join("csrf-protected.txt").exists());

    for csrf_token in [None, Some("invalid")] {
        let mut request = with_resume_upload_headers(
            context.request(Method::PATCH, file_url.clone()),
            upload_id,
            upload_length,
            initial.len() as u64,
        )
        .body(" blocked patch");
        if let Some(csrf_token) = csrf_token {
            request = request.header(CSRF_HEADER, csrf_token);
        }
        let response = request.send()?;
        assert_eq!(response.status(), 403, "PATCH csrf={csrf_token:?}");
        assert_eq!(response_json(response)?["code"], "auth.csrf_rejected");
        assert!(!server.path().join("csrf-protected.txt").exists());
    }

    let response = with_resume_upload_headers(
        context.request(Method::PATCH, file_url.clone()),
        upload_id,
        upload_length,
        initial.len() as u64,
    )
    .header(CSRF_HEADER, &context.csrf_token)
    .body(remainder)
    .send()?;
    assert_eq!(response.status(), 204);
    assert_eq!(
        std::fs::read_to_string(server.path().join("csrf-protected.txt"))?,
        "created and appended"
    );
    let delete_revision = preflight_upload_target(&server, "/csrf-protected.txt")?
        .revision
        .ok_or("csrf delete target has no revision")?;

    for csrf_token in [None, Some("invalid")] {
        let mut request = context.request(Method::DELETE, file_url.clone());
        if let Some(csrf_token) = csrf_token {
            request = request.header(CSRF_HEADER, csrf_token);
        }
        let response = request.send()?;
        assert_eq!(response.status(), 403, "DELETE csrf={csrf_token:?}");
        assert_eq!(response_json(response)?["code"], "auth.csrf_rejected");
        assert!(server.path().join("csrf-protected.txt").is_file());
    }

    let response = context
        .request(Method::DELETE, file_url)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .header("If-Match", format!("\"{delete_revision}\""))
        .send()?;
    assert_eq!(response.status(), 204);
    assert!(!server.path().join("csrf-protected.txt").exists());
    Ok(())
}

#[rstest]
fn browser_api_limits_fixed_and_streamed_bodies(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let oversized = "x".repeat(17 * 1024);

    let fixed = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "application/json")
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .body(oversized.clone())
        .send()?;
    assert_eq!(fixed.status(), 413);

    let streamed = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "application/json")
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .body(reqwest::blocking::Body::new(Cursor::new(
            oversized.into_bytes(),
        )))
        .send()?;
    assert_eq!(streamed.status(), 413);
    Ok(())
}

#[rstest]
fn unsupported_methods_are_rejected(server: TestServer) -> Result<(), Error> {
    let url = server.url().join("test.html")?;
    let context = browser_context(&server, "")?;
    for method in [b"OPTIONS".as_slice(), b"BREW"] {
        let response = context
            .request(Method::from_bytes(method)?, url.clone())
            .header(CSRF_HEADER, &context.csrf_token)
            .send()?;
        assert_eq!(
            response.status(),
            405,
            "method={}",
            String::from_utf8_lossy(method)
        );
    }
    Ok(())
}

#[rstest]
fn mkdir_rejects_outside_symlink_before_creating(server: TestServer) -> Result<(), Error> {
    use std::os::unix::fs::symlink;

    let outside = assert_fs::TempDir::new()?;
    symlink(outside.path(), server.path().join("outside-link"))?;
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "mkdir",
        json!({"path": "/outside-link/must-not-exist"}),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 400);
    assert!(!outside.path().join("must-not-exist").exists());
    Ok(())
}
