#[path = "support/fixtures.rs"]
mod fixtures;
#[path = "support/utils.rs"]
mod utils;

use fixtures::{
    Error, TEST_ACCOUNT, TestServer, UPLOAD_STAGE_DIRECTORY, preflight_upload_target, server,
    with_resume_upload_headers, with_upload_headers, with_upload_overwrite_headers,
};
use reqwest::blocking::{RequestBuilder, Response};
use reqwest::header::{COOKIE, HeaderValue};
use reqwest::{IntoUrl, Method, Url};
use rstest::rstest;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::Cursor;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use uuid::Uuid;

const CSRF_HEADER: &str = "x-csrf-token";

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
            .header("origin", self.server.url().origin().ascii_serialization())
            .header("sec-fetch-site", "same-origin")
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
    assert_eq!(data.as_object().ok_or("Invalid index data")?.len(), 2);
    assert!(data.get("href").is_some() && data.get("dir_exists").is_some());
    let csrf_token = server
        .request(Method::POST, server.url())
        .build()?
        .headers()
        .get(CSRF_HEADER)
        .ok_or("Missing CSRF token")?
        .to_str()?
        .to_string();
    let api_base = server.url().join("__dufs__/api/")?;
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
    let body = enrich_relocation_revisions(context, action, body)?;
    post_json_raw(context, action, body, csrf_token)
}

fn post_json_raw(
    context: &BrowserContext<'_>,
    action: &str,
    body: Value,
    csrf_token: Option<&str>,
) -> Result<Response, Error> {
    let mut request = context
        .request(Method::POST, context.api_base.join(action)?)
        .header("content-type", "application/json")
        .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
        .body(body.to_string());
    if let Some(csrf_token) = csrf_token {
        request = request.header(CSRF_HEADER, csrf_token);
    }
    Ok(request.send()?)
}

fn post_json_with_operation(
    context: &BrowserContext<'_>,
    action: &str,
    body: Value,
    operation_id: Uuid,
) -> Result<Response, Error> {
    let body = enrich_relocation_revisions(context, action, body)?;
    Ok(context
        .request(Method::POST, context.api_base.join(action)?)
        .header("content-type", "application/json")
        .header(CSRF_HEADER, &context.csrf_token)
        .header("X-Dufs-Operation-Id", operation_id.to_string())
        .body(body.to_string())
        .send()?)
}

fn enrich_relocation_revisions(
    context: &BrowserContext<'_>,
    action: &str,
    mut body: Value,
) -> Result<Value, Error> {
    if !matches!(action, "move" | "rename") {
        return Ok(body);
    }
    let Some(fields) = body.as_object_mut() else {
        return Ok(body);
    };
    let Some(source) = fields
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(body);
    };
    if !fields.contains_key("source_revision") {
        let revision = preflight_upload_target(context.server, &source)?
            .revision
            .unwrap_or_else(|| "0".repeat(64));
        fields.insert("source_revision".to_owned(), Value::String(revision));
    }
    if fields.get("overwrite").and_then(Value::as_bool) == Some(true)
        && !fields.contains_key("destination_revision")
    {
        let destination = match action {
            "move" => fields
                .get("directory")
                .and_then(Value::as_str)
                .zip(source.rsplit('/').next())
                .map(|(directory, name)| logical_test_child(directory, name)),
            "rename" => fields.get("name").and_then(Value::as_str).map(|name| {
                let parent = source
                    .rsplit_once('/')
                    .map(|(parent, _)| parent)
                    .unwrap_or_default();
                logical_test_child(if parent.is_empty() { "/" } else { parent }, name)
            }),
            _ => None,
        };
        if let Some(destination) = destination {
            let revision = preflight_upload_target(context.server, &destination)?
                .revision
                .unwrap_or_else(|| "0".repeat(64));
            fields.insert("destination_revision".to_owned(), Value::String(revision));
        }
    }
    Ok(body)
}

fn logical_test_child(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{directory}/{name}")
    }
}

fn response_json(response: Response) -> Result<Value, Error> {
    Ok(serde_json::from_str(&response.text()?)?)
}

fn upload_preflight_raw(
    context: &BrowserContext<'_>,
    method: Method,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Result<Response, Error> {
    let mut request = context
        .request(method, context.api_base.join("upload/preflight")?)
        .header(CSRF_HEADER, &context.csrf_token)
        .body(body);
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    Ok(request.send()?)
}

fn assert_api_problem(
    response: Response,
    expected_status: reqwest::StatusCode,
    expected_code: &str,
) -> Result<(), Error> {
    assert_eq!(response.status(), expected_status);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let problem = response_json(response)?;
    assert_eq!(problem["status"], expected_status.as_u16());
    assert_eq!(problem["code"], expected_code);
    Ok(())
}

type UploadCheckpointRow = (Vec<u8>, Vec<u8>, i64, i64, i64);

fn sqlite_upload_checkpoint(
    database: &Path,
    upload_id: Uuid,
) -> Result<UploadCheckpointRow, Error> {
    let connection = Connection::open(database)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM upload_sessions WHERE upload_id = ?1",
        params![upload_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1, "upload ID must have exactly one durable row");
    Ok(connection.query_row(
        "SELECT target_path, stage_path, upload_length, durable_offset, state
           FROM upload_sessions
          WHERE upload_id = ?1",
        params![upload_id.as_bytes().as_slice()],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?)
}

fn has_internal_delete_trash(root: &Path) -> Result<bool, Error> {
    Ok(std::fs::read_dir(root)?
        .filter_map(Result::ok)
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".dufs-upload-delete-") && name.ends_with(".trash")
        }))
}

fn assert_concurrent_no_replace_renames(
    context: &BrowserContext<'_>,
    server: &TestServer,
) -> Result<(), Error> {
    std::fs::write(server.path().join("source-a.txt"), "source-a")?;
    std::fs::write(server.path().join("source-b.txt"), "source-b")?;
    let source_a_revision = preflight_upload_target(server, "/source-a.txt")?
        .revision
        .ok_or("missing source-a revision")?;
    let source_b_revision = preflight_upload_target(server, "/source-b.txt")?
        .revision
        .ok_or("missing source-b revision")?;
    let request = |source: &str, source_revision: &str| {
        context
            .request(Method::POST, context.api_base.join("rename").unwrap())
            .header(CSRF_HEADER, &context.csrf_token)
            .header("content-type", "application/json")
            .header("X-Dufs-Operation-Id", Uuid::new_v4().to_string())
            .body(
                json!({
                    "source": source,
                    "source_revision": source_revision,
                    "name": "winner.txt",
                    "overwrite": false
                })
                .to_string(),
            )
    };
    let request_a = request("/source-a.txt", &source_a_revision);
    let request_b = request("/source-b.txt", &source_b_revision);
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

#[path = "browser_api/create.rs"]
mod create;
#[path = "browser_api/durability.rs"]
mod durability;
#[path = "browser_api/jobs.rs"]
mod jobs;
#[path = "browser_api/relocation.rs"]
mod relocation;
#[path = "browser_api/request_validation.rs"]
mod request_validation;
#[path = "browser_api/upload_preflight.rs"]
mod upload_preflight;
