#[path = "support/fixtures.rs"]
mod fixtures;

use fixtures::{ADMIN_ACCOUNT, Error, TEST_PASSWORD, TestServer, USER_ACCOUNT, server};
use rstest::rstest;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Instant;

fn response_json(response: reqwest::blocking::Response) -> Result<Value, Error> {
    Ok(serde_json::from_str(&response.text()?)?)
}

fn assert_problem(
    response: reqwest::blocking::Response,
    expected_status: u16,
    expected_code: &str,
    expected_detail: &str,
    expected_recovery: Option<&str>,
) -> Result<(), Error> {
    assert_eq!(response.status().as_u16(), expected_status);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
    let problem = response_json(response)?;
    assert_eq!(problem["type"], format!("urn:dufs:problem:{expected_code}"));
    assert_eq!(problem["status"], expected_status);
    assert_eq!(problem["code"], expected_code);
    assert_eq!(problem["detail"], expected_detail);
    assert!(problem.get("message").is_none());
    match expected_recovery {
        Some(value) => assert_eq!(problem["recovery"], value),
        None => assert!(problem.get("recovery").is_none()),
    }
    Ok(())
}

#[rstest]
fn paginated_listing_has_no_duplicates_or_omissions(server: TestServer) -> Result<(), Error> {
    const CREATED: usize = 1_205;
    for index in 0..CREATED {
        std::fs::write(
            server.path().join(format!("large-{index:04}.txt")),
            index.to_string(),
        )?;
    }

    let mut cursor = None;
    let mut names = Vec::new();
    loop {
        let mut parameters = vec![("limit", "37"), ("sort", "name"), ("order", "asc")];
        if let Some(value) = cursor.as_deref() {
            parameters.push(("cursor", value));
        }
        let response = server.list_api("/", &parameters)?;
        assert_eq!(response.status(), 200);
        let data = response_json(response)?;
        names.extend(
            data["paths"]
                .as_array()
                .ok_or("List response has no paths")?
                .iter()
                .filter_map(|item| item["name"].as_str())
                .filter(|name| name.starts_with("large-"))
                .map(ToOwned::to_owned),
        );
        cursor = data["next_cursor"].as_str().map(ToOwned::to_owned);
        if cursor.is_none() {
            break;
        }
    }

    let unique = names.iter().collect::<HashSet<_>>();
    assert_eq!(names.len(), CREATED);
    assert_eq!(unique.len(), CREATED);
    assert_eq!(names.first().map(String::as_str), Some("large-0000.txt"));
    assert_eq!(names.last().map(String::as_str), Some("large-1204.txt"));
    Ok(())
}

#[rstest]
fn cursor_is_rejected_after_directory_changes(server: TestServer) -> Result<(), Error> {
    let first = response_json(server.list_api("/", &[("limit", "1")])?)?;
    let cursor = first["next_cursor"]
        .as_str()
        .ok_or("Expected a continuation cursor")?
        .to_string();

    std::fs::write(server.path().join("created-between-pages.txt"), "changed")?;
    let response = server.list_api("/", &[("limit", "1"), ("cursor", &cursor)])?;
    assert_problem(
        response,
        409,
        "directory_changed",
        "Directory changed; restart listing",
        Some("refresh_target"),
    )?;
    Ok(())
}

#[rstest]
fn cursor_cannot_be_reused_with_different_sorting(server: TestServer) -> Result<(), Error> {
    let first = response_json(server.list_api("/", &[("limit", "1"), ("sort", "name")])?)?;
    let cursor = first["next_cursor"]
        .as_str()
        .ok_or("Expected a continuation cursor")?;

    let response = server.list_api("/", &[("limit", "1"), ("sort", "size"), ("cursor", cursor)])?;
    assert_problem(
        response,
        400,
        "invalid_list_cursor",
        "Invalid list cursor",
        None,
    )?;
    Ok(())
}

#[rstest]
fn cursor_is_bound_to_the_page_size(server: TestServer) -> Result<(), Error> {
    let first = response_json(server.list_api("/", &[("limit", "1")])?)?;
    let cursor = first["next_cursor"]
        .as_str()
        .ok_or("Expected a continuation cursor")?;

    let response = server.list_api("/", &[("limit", "2"), ("cursor", cursor)])?;
    assert_problem(
        response,
        400,
        "invalid_list_cursor",
        "Invalid list cursor",
        None,
    )?;
    Ok(())
}

#[rstest]
fn cursor_is_bound_to_the_authenticated_account(
    #[with(&["--auth", USER_ACCOUNT, "--auth", ADMIN_ACCOUNT])] server: TestServer,
) -> Result<(), Error> {
    let user = server.login("user", TEST_PASSWORD)?;
    let admin = server.login("admin", TEST_PASSWORD)?;
    let mut first_url = server.url().join("__dufs__/api/list")?;
    first_url
        .query_pairs_mut()
        .append_pair("path", "/")
        .append_pair("limit", "1");
    let first = response_json(server.get_with(&user, first_url)?)?;
    let cursor = first["next_cursor"]
        .as_str()
        .ok_or("Expected a continuation cursor")?;

    let mut replay_url = server.url().join("__dufs__/api/list")?;
    replay_url
        .query_pairs_mut()
        .append_pair("path", "/")
        .append_pair("limit", "1")
        .append_pair("cursor", cursor);
    let replay = server.get_with(&admin, replay_url)?;
    assert_problem(
        replay,
        400,
        "invalid_list_cursor",
        "Invalid list cursor",
        None,
    )?;
    Ok(())
}

#[rstest]
fn cursor_missing_after_restart_is_an_explicit_conflict(
    mut server: TestServer,
) -> Result<(), Error> {
    let first = response_json(server.list_api("/", &[("limit", "1")])?)?;
    let cursor = first["next_cursor"]
        .as_str()
        .ok_or("Expected a continuation cursor")?
        .to_string();

    server.restart_with_default_auth();
    let response = server.list_api("/", &[("limit", "1"), ("cursor", &cursor)])?;
    assert_problem(
        response,
        409,
        "list_cursor_unavailable",
        "List cursor expired or is unavailable; restart listing",
        Some("refresh_target"),
    )?;
    Ok(())
}

#[rstest]
fn recursive_search_uses_one_immutable_result_snapshot(server: TestServer) -> Result<(), Error> {
    let nested = server.path().join("snapshot-nested");
    std::fs::create_dir(&nested)?;
    for name in [
        "snapshot-hit-01.txt",
        "snapshot-hit-02.txt",
        "snapshot-hit-03.txt",
    ] {
        std::fs::write(nested.join(name), name)?;
    }

    let first = response_json(server.list_api(
        "/",
        &[
            ("limit", "1"),
            ("sort", "name"),
            ("order", "asc"),
            ("q", "snapshot-hit-"),
        ],
    )?)?;
    let mut names = first["paths"]
        .as_array()
        .ok_or("List response has no paths")?
        .iter()
        .filter_map(|item| item["name"].as_str())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut cursor = first["next_cursor"]
        .as_str()
        .ok_or("Expected a continuation cursor")?
        .to_string();

    std::fs::remove_file(nested.join("snapshot-hit-02.txt"))?;
    std::fs::write(nested.join("snapshot-hit-00-new.txt"), "new")?;

    loop {
        let data = response_json(server.list_api(
            "/",
            &[
                ("limit", "1"),
                ("sort", "name"),
                ("order", "asc"),
                ("q", "snapshot-hit-"),
                ("cursor", &cursor),
            ],
        )?)?;
        names.extend(
            data["paths"]
                .as_array()
                .ok_or("List response has no paths")?
                .iter()
                .filter_map(|item| item["name"].as_str())
                .map(ToOwned::to_owned),
        );
        let Some(next) = data["next_cursor"].as_str() else {
            break;
        };
        cursor = next.to_string();
    }

    assert_eq!(
        names,
        [
            "snapshot-nested/snapshot-hit-01.txt",
            "snapshot-nested/snapshot-hit-02.txt",
            "snapshot-nested/snapshot-hit-03.txt",
        ]
    );
    Ok(())
}

#[rstest]
fn search_limit_counts_unicode_characters_consistently_with_the_browser(
    server: TestServer,
) -> Result<(), Error> {
    let accepted = "文".repeat(128);
    let response = server.list_api("/", &[("q", &accepted)])?;
    assert_eq!(response.status(), 200);

    let rejected = "文".repeat(129);
    let response = server.list_api("/", &[("q", &rejected)])?;
    assert_problem(
        response,
        400,
        "search_query_too_long",
        "Search query is too long",
        None,
    )?;
    Ok(())
}

/// Reproducible local performance probe. It is ignored by the normal suite
/// because creating 100,000 real directory entries is intentionally costly.
#[rstest]
#[ignore = "manual large-directory benchmark"]
fn benchmark_one_hundred_thousand_entries(server: TestServer) -> Result<(), Error> {
    const CREATED: usize = 100_000;
    let benchmark = server.path().join("benchmark");
    std::fs::create_dir(&benchmark)?;
    for index in 0..CREATED {
        std::fs::write(benchmark.join(format!("bench-{index:06}")), [])?;
    }

    let started = Instant::now();
    let response = server.list_api("/benchmark", &[("limit", "500")])?;
    assert_eq!(response.status(), 200);
    let data = response_json(response)?;
    assert_eq!(data["paths"].as_array().map(Vec::len), Some(500));
    assert!(data["next_cursor"].is_string());
    let elapsed = started.elapsed();
    eprintln!("100000-entry first page completed in {elapsed:?}");
    if let Some(max_millis) = std::env::var_os("DUFS_BENCHMARK_MAX_MILLIS") {
        let max_millis = max_millis
            .to_str()
            .ok_or("DUFS_BENCHMARK_MAX_MILLIS is not UTF-8")?
            .parse::<u64>()?;
        assert!(
            elapsed <= std::time::Duration::from_millis(max_millis),
            "100000-entry first page took {elapsed:?}, exceeding the {max_millis}ms baseline"
        );
    }
    Ok(())
}
