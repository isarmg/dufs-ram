#[path = "support/fixtures.rs"]
mod fixtures;
#[path = "support/utils.rs"]
mod utils;

use fixtures::{
    BIN_FILE, Error, TestServer, UPLOAD_STAGE_DIRECTORY, preflight_upload_target, server,
    with_new_upload_headers, with_new_upload_overwrite_headers, with_resume_upload_headers,
    with_upload_headers, with_upload_overwrite_headers,
};
use reqwest::header::HeaderMap;
use rstest::rstest;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
};
use uuid::Uuid;

const INDEX_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";
const PERMISSIONS_POLICY: &str = "camera=(), microphone=(), geolocation=(), payment=(), usb=()";

fn assert_index_security_headers(headers: &HeaderMap) {
    assert_eq!(headers.get("cache-control").unwrap(), "private, no-store");
    assert_eq!(headers.get("content-security-policy").unwrap(), INDEX_CSP);
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
    assert_eq!(
        headers.get("permissions-policy").unwrap(),
        PERMISSIONS_POLICY
    );
}

fn assert_upload_problem_body(
    response: reqwest::blocking::Response,
    expected_code: &str,
    expected_detail: &str,
    expected_recovery: &str,
) -> Result<(), Error> {
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let problem: serde_json::Value = serde_json::from_str(&response.text()?)?;
    assert_eq!(problem["type"], format!("urn:dufs:problem:{expected_code}"));
    assert_eq!(problem["code"], expected_code);
    assert_eq!(problem["detail"], expected_detail);
    assert!(problem.get("message").is_none());
    assert_eq!(problem["recovery"], expected_recovery);
    Ok(())
}

fn assert_problem_code(
    response: reqwest::blocking::Response,
    expected_code: &str,
) -> Result<(), Error> {
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let status = response.status().as_u16();
    let problem: serde_json::Value = serde_json::from_str(&response.text()?)?;
    assert_eq!(problem["type"], format!("urn:dufs:problem:{expected_code}"));
    assert_eq!(problem["status"], status);
    assert_eq!(problem["code"], expected_code);
    assert!(problem["detail"].is_string());
    assert!(problem.get("message").is_none());
    Ok(())
}

#[path = "http/delete.rs"]
mod delete;
#[path = "http/download.rs"]
mod download;
#[path = "http/listing.rs"]
mod listing;
#[path = "http/resumable_upload.rs"]
mod resumable_upload;
#[path = "http/upload.rs"]
mod upload;
