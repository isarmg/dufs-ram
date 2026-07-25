mod fixtures;

use fixtures::{Error, TestServer, server, with_new_upload_headers};
use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE};
use reqwest::{Method, StatusCode};
use rstest::rstest;
use serde_json::json;
use sha2::{Digest, Sha256};

const LONG_LIVED_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const PRIVATE_NO_STORE: &str = "private, no-store";

#[rstest]
fn embedded_assets_are_content_addressed_and_cacheable(server: TestServer) -> Result<(), Error> {
    verify_embedded_assets(&server, "", "/")
}

#[rstest]
fn prefixed_embedded_assets_are_content_addressed_and_cacheable(
    #[with(&["--path-prefix", "xyz"])] server: TestServer,
) -> Result<(), Error> {
    verify_embedded_assets(&server, "xyz/", "/xyz/")
}

fn verify_embedded_assets(
    server: &TestServer,
    page_path: &str,
    expected_uri_prefix: &str,
) -> Result<(), Error> {
    let page = server
        .get(server.url().join(page_path)?)?
        .error_for_status()?
        .text()?;
    let index_js = extract_asset_url(&page, "src", "index.js")?;
    let index_css = extract_asset_url(&page, "href", "index.css")?;
    let favicon = extract_asset_url(&page, "href", "favicon.ico")?;

    let asset_prefix = shared_asset_prefix([&index_js, &index_css, &favicon], expected_uri_prefix)?;
    let advertised_digest = asset_prefix
        .strip_prefix(&format!("{expected_uri_prefix}__dufs_assets_"))
        .and_then(|value| value.strip_suffix('/'))
        .ok_or("Embedded asset prefix has an unexpected form")?;

    let assets = [
        (
            "index.js",
            index_js,
            "application/javascript; charset=UTF-8",
        ),
        ("index.css", index_css, "text/css; charset=UTF-8"),
        ("favicon.ico", favicon, "image/x-icon"),
        (
            "modules/api.js",
            format!("{asset_prefix}modules/api.js"),
            "application/javascript; charset=UTF-8",
        ),
        (
            "modules/app.js",
            format!("{asset_prefix}modules/app.js"),
            "application/javascript; charset=UTF-8",
        ),
        (
            "modules/dom.js",
            format!("{asset_prefix}modules/dom.js"),
            "application/javascript; charset=UTF-8",
        ),
        (
            "modules/listing.js",
            format!("{asset_prefix}modules/listing.js"),
            "application/javascript; charset=UTF-8",
        ),
        (
            "modules/operations.js",
            format!("{asset_prefix}modules/operations.js"),
            "application/javascript; charset=UTF-8",
        ),
        (
            "modules/path.js",
            format!("{asset_prefix}modules/path.js"),
            "application/javascript; charset=UTF-8",
        ),
        (
            "modules/upload.js",
            format!("{asset_prefix}modules/upload.js"),
            "application/javascript; charset=UTF-8",
        ),
    ];
    let mut digest = Sha256::new();
    for (name, path, expected_content_type) in assets {
        let response = server.get(server.url().join(&path)?)?;
        assert_eq!(response.status(), StatusCode::OK, "asset={path}");
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(expected_content_type),
            "asset={path}"
        );
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some(LONG_LIVED_CACHE_CONTROL),
            "asset={path}"
        );

        let contents = response.bytes()?;
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((contents.len() as u64).to_be_bytes());
        digest.update(&contents);
    }
    assert_eq!(
        hex::encode(digest.finalize()),
        advertised_digest,
        "Embedded asset prefix must be the SHA-256 digest of the served assets"
    );

    let missing_path = format!("{asset_prefix}missing.js");
    let missing = server.get(server.url().join(&missing_path)?)?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        missing
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some(PRIVATE_NO_STORE)
    );
    assert_eq!(missing.text()?, "Not Found");

    let reserved_component = asset_prefix
        .strip_prefix(expected_uri_prefix)
        .and_then(|value| value.strip_suffix('/'))
        .ok_or("Embedded asset URL has an invalid reserved component")?;
    let upload = with_new_upload_headers(
        server.request(
            Method::PUT,
            server.url().join(&format!("{asset_prefix}user-file.txt"))?,
        ),
        7,
    )
    .body("blocked")
    .send()?;
    assert_eq!(upload.status(), StatusCode::NOT_FOUND);

    let mkdir = server
        .request(
            Method::POST,
            server
                .url()
                .join(&format!("{expected_uri_prefix}__dufs__/api/mkdir"))?,
        )
        .header(CONTENT_TYPE, "application/json")
        .body(json!({"path": format!("/{reserved_component}/directory")}).to_string())
        .send()?;
    assert_eq!(mkdir.status(), StatusCode::BAD_REQUEST);
    assert!(!server.path().join(reserved_component).exists());
    Ok(())
}

fn extract_asset_url(page: &str, attribute: &str, filename: &str) -> Result<String, Error> {
    let marker = format!(r#"{attribute}=""#);
    let expected_suffix = format!("/{filename}");
    let mut found = None;

    for (start, _) in page.match_indices(&marker) {
        let value = &page[start + marker.len()..];
        let Some(end) = value.find('"') else {
            return Err(format!("The {attribute} attribute is missing its closing quote").into());
        };
        let value = &value[..end];
        if !value.ends_with(&expected_suffix) {
            continue;
        }
        if found.replace(value.to_string()).is_some() {
            return Err(format!("Directory page contains duplicate {filename} URLs").into());
        }
    }

    let path = found.ok_or_else(|| format!("Directory page is missing the {filename} URL"))?;
    if !path.starts_with('/') {
        return Err(format!("Embedded asset URL is not absolute: {path}").into());
    }
    Ok(path)
}

fn shared_asset_prefix(paths: [&str; 3], expected_uri_prefix: &str) -> Result<String, Error> {
    let filenames = ["index.js", "index.css", "favicon.ico"];
    let mut shared_prefix = None;

    for (path, filename) in paths.into_iter().zip(filenames) {
        let prefix = path
            .strip_suffix(filename)
            .ok_or_else(|| format!("Embedded asset URL does not end with {filename}: {path}"))?;
        if let Some(shared_prefix) = shared_prefix {
            if shared_prefix != prefix {
                return Err("Directory page assets do not share one content digest prefix".into());
            }
        } else {
            shared_prefix = Some(prefix);
        }
    }

    let shared_prefix = shared_prefix.ok_or("Directory page does not contain embedded assets")?;
    let digest = shared_prefix
        .strip_prefix(&format!("{expected_uri_prefix}__dufs_assets_"))
        .and_then(|value| value.strip_suffix('/'))
        .ok_or_else(|| format!("Unexpected embedded asset prefix: {shared_prefix}"))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(format!("Embedded asset digest is not lowercase SHA-256: {digest}").into());
    }

    Ok(shared_prefix.to_string())
}
