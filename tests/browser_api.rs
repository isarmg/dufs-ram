mod fixtures;
mod utils;

use fixtures::{Error, TestServer, server, with_resume_upload_headers, with_upload_headers};
use reqwest::blocking::{RequestBuilder, Response};
use reqwest::header::{COOKIE, HeaderValue};
use reqwest::{IntoUrl, Method, Url};
use rstest::rstest;
use serde_json::{Value, json};
use std::io::Cursor;
use std::sync::{Arc, Barrier};
use uuid::Uuid;

const CSRF_HEADER: &str = "x-dufs-csrf-token";
const AUTH_ERROR_HEADER: &str = "x-dufs-auth-error";

struct BrowserContext<'a> {
    server: &'a TestServer,
    api_base: Url,
    cookie: HeaderValue,
    csrf_token: String,
}

impl BrowserContext<'_> {
    fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        self.server
            .raw_request(method, url)
            .header(COOKIE, &self.cookie)
    }
}

fn browser_context<'a>(
    server: &'a TestServer,
    page_path: &str,
) -> Result<BrowserContext<'a>, Error> {
    let page_url = server.url().join(page_path)?;
    let cookie = server
        .request(Method::GET, page_url.clone())
        .build()?
        .headers()
        .get(COOKIE)
        .cloned()
        .ok_or("Missing authenticated session cookie")?;
    let response = server.get(page_url)?;
    assert_eq!(response.status(), 200);
    let cache_control = response
        .headers()
        .get("cache-control")
        .ok_or("Missing Cache-Control")?
        .to_str()?;
    let directives = cache_control.split(',').map(str::trim).collect::<Vec<_>>();
    assert!(directives.contains(&"private"));
    assert!(directives.contains(&"no-store"));
    let data = utils::retrieve_json(&response.text()?).ok_or("Missing index data")?;
    let uri_prefix = data["uri_prefix"].as_str().ok_or("Missing URI prefix")?;
    let csrf_token = data["csrf_token"]
        .as_str()
        .ok_or("Missing CSRF token")?
        .to_string();
    let api_base = server.url().join(&format!(
        "{}__dufs__/api/",
        uri_prefix.trim_start_matches('/')
    ))?;
    Ok(BrowserContext {
        server,
        api_base,
        cookie,
        csrf_token,
    })
}

fn post_json(
    context: &BrowserContext<'_>,
    action: &str,
    body: Value,
    csrf_token: Option<&str>,
) -> Result<Response, Error> {
    let mut request = context
        .request(Method::POST, context.api_base.join(action)?)
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(csrf_token) = csrf_token {
        request = request.header(CSRF_HEADER, csrf_token);
    }
    Ok(request.send()?)
}

fn assert_concurrent_no_replace_moves(
    context: &BrowserContext<'_>,
    server: &TestServer,
) -> Result<(), Error> {
    std::fs::write(server.path().join("source-a.txt"), "source-a")?;
    std::fs::write(server.path().join("source-b.txt"), "source-b")?;
    let request = |source: &str| {
        context
            .request(Method::POST, context.api_base.join("move").unwrap())
            .header(CSRF_HEADER, &context.csrf_token)
            .header("content-type", "application/json")
            .body(
                json!({
                    "source": source,
                    "destination": "/winner.txt",
                    "overwrite": false
                })
                .to_string(),
            )
    };
    let request_a = request("/source-a.txt");
    let request_b = request("/source-b.txt");
    let barrier = Arc::new(Barrier::new(3));

    let (response_a, response_b) = std::thread::scope(|scope| {
        let barrier_a = Arc::clone(&barrier);
        let handle_a = scope.spawn(move || {
            barrier_a.wait();
            request_a.send()
        });
        let barrier_b = Arc::clone(&barrier);
        let handle_b = scope.spawn(move || {
            barrier_b.wait();
            request_b.send()
        });
        barrier.wait();
        (
            handle_a.join().expect("first move thread panicked"),
            handle_b.join().expect("second move thread panicked"),
        )
    });
    let status_a = response_a?.status();
    let status_b = response_b?.status();

    assert!(
        matches!(
            (status_a.as_u16(), status_b.as_u16()),
            (204, 409) | (409, 204)
        ),
        "unexpected statuses: first={status_a}, second={status_b}"
    );
    let (winner, loser, content) = if status_a == 204 {
        ("source-a.txt", "source-b.txt", "source-a")
    } else {
        ("source-b.txt", "source-a.txt", "source-b")
    };
    assert!(!server.path().join(winner).exists());
    assert!(server.path().join(loser).is_file());
    assert_eq!(
        std::fs::read_to_string(server.path().join("winner.txt"))?,
        content
    );
    Ok(())
}

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
fn mkdir_existing_path_conflicts(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "mkdir",
        json!({"path": "/dir1"}),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 409);
    assert_eq!(response.text()?, "Path already exists");
    Ok(())
}

#[rstest]
fn move_file_and_create_destination_parent(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "move",
        json!({
            "source": "/test.html",
            "destination": "/moved/test.html",
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
fn move_directory(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "move",
        json!({
            "source": "/dir1",
            "destination": "/renamed-dir",
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
fn move_requires_explicit_overwrite_and_no_replace_has_one_winner(
    server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let request = json!({
        "source": "/test.html",
        "destination": "/index.html",
        "overwrite": false
    });
    let response = post_json(&context, "move", request.clone(), Some(&context.csrf_token))?;
    assert_eq!(response.status(), 409);
    assert!(server.path().join("test.html").is_file());
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is index.html"
    );

    let mut overwrite = request;
    overwrite["overwrite"] = Value::Bool(true);
    let response = post_json(&context, "move", overwrite, Some(&context.csrf_token))?;
    assert_eq!(response.status(), 204);
    assert!(!server.path().join("test.html").exists());
    assert_eq!(
        std::fs::read_to_string(server.path().join("index.html"))?,
        "This is test.html"
    );
    assert_concurrent_no_replace_moves(&context, &server)?;
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
            "destination": "/new",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(missing.status(), 404);

    let directory_overwrite = post_json(
        &context,
        "move",
        json!({
            "source": "/dir1",
            "destination": "/dir2",
            "overwrite": true
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(directory_overwrite.status(), 409);
    assert!(server.path().join("dir1").is_dir());
    assert!(server.path().join("dir2").is_dir());
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
        json!({"source": "/dir1", "destination": "/dir1"}),
        Some(&context.csrf_token),
    )?;
    assert_eq!(same_path.status(), 400);

    let descendant = post_json(
        &context,
        "move",
        json!({
            "source": "/dir1",
            "destination": "/dir1/child",
            "overwrite": false
        }),
        Some(&context.csrf_token),
    )?;
    assert_eq!(descendant.status(), 409);
    Ok(())
}

#[rstest]
fn logical_paths_do_not_percent_decode(server: TestServer) -> Result<(), Error> {
    std::fs::write(server.path().join("literal%2F.txt"), "literal percent")?;
    let context = browser_context(&server, "")?;
    let response = post_json(
        &context,
        "move",
        json!({
            "source": "/literal%2F.txt",
            "destination": "/moved%2F.txt",
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

#[rstest]
fn browser_api_requires_csrf_and_json(server: TestServer) -> Result<(), Error> {
    let context = browser_context(&server, "")?;
    let missing_csrf = post_json(&context, "mkdir", json!({"path": "/blocked"}), None)?;
    assert_eq!(missing_csrf.status(), 403);
    assert_eq!(
        missing_csrf.headers().get(AUTH_ERROR_HEADER).unwrap(),
        "csrf"
    );
    assert!(!server.path().join("blocked").exists());

    let invalid_csrf = post_json(
        &context,
        "mkdir",
        json!({"path": "/blocked"}),
        Some("invalid"),
    )?;
    assert_eq!(invalid_csrf.status(), 403);
    assert_eq!(
        invalid_csrf.headers().get(AUTH_ERROR_HEADER).unwrap(),
        "csrf"
    );

    let wrong_type = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "text/plain")
        .body(r#"{"path":"/blocked"}"#)
        .send()?;
    assert_eq!(wrong_type.status(), 415);

    let malformed = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "application/json")
        .body("{")
        .send()?;
    assert_eq!(malformed.status(), 400);
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
        assert_eq!(response.headers().get(AUTH_ERROR_HEADER).unwrap(), "csrf");
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
        assert_eq!(response.headers().get(AUTH_ERROR_HEADER).unwrap(), "csrf");
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

    for csrf_token in [None, Some("invalid")] {
        let mut request = context.request(Method::DELETE, file_url.clone());
        if let Some(csrf_token) = csrf_token {
            request = request.header(CSRF_HEADER, csrf_token);
        }
        let response = request.send()?;
        assert_eq!(response.status(), 403, "DELETE csrf={csrf_token:?}");
        assert_eq!(response.headers().get(AUTH_ERROR_HEADER).unwrap(), "csrf");
        assert!(server.path().join("csrf-protected.txt").is_file());
    }

    let response = context
        .request(Method::DELETE, file_url)
        .header(CSRF_HEADER, &context.csrf_token)
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
        .body(oversized.clone())
        .send()?;
    assert_eq!(fixed.status(), 413);

    let streamed = context
        .request(Method::POST, context.api_base.join("mkdir")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .header("content-type", "application/json")
        .body(reqwest::blocking::Body::new(Cursor::new(
            oversized.into_bytes(),
        )))
        .send()?;
    assert_eq!(streamed.status(), 413);
    Ok(())
}

#[rstest]
fn browser_api_respects_path_prefix(
    #[with(&["--path-prefix", "xyz"])] server: TestServer,
) -> Result<(), Error> {
    let context = browser_context(&server, "xyz/")?;
    assert!(context.api_base.path().starts_with("/xyz/__dufs__/api/"));
    let response = post_json(
        &context,
        "mkdir",
        json!({"path": "/prefixed"}),
        Some(&context.csrf_token),
    )?;
    assert_eq!(response.status(), 201);
    assert!(server.path().join("prefixed").is_dir());
    Ok(())
}

#[rstest]
fn unsupported_methods_are_rejected(server: TestServer) -> Result<(), Error> {
    let url = server.url().join("test.html")?;
    let context = browser_context(&server, "")?;
    for method in [b"OPTIONS".as_slice(), b"BREW"] {
        let response = context
            .request(Method::from_bytes(method)?, url.clone())
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
