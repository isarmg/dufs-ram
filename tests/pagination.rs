mod fixtures;

use fixtures::{Error, TestServer, server};
use rstest::rstest;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Instant;

fn response_json(response: reqwest::blocking::Response) -> Result<Value, Error> {
    Ok(serde_json::from_str(&response.text()?)?)
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
    assert_eq!(response.status(), 409);
    assert_eq!(response.text()?, "Directory changed; restart listing");
    Ok(())
}

#[rstest]
fn cursor_cannot_be_reused_with_different_sorting(server: TestServer) -> Result<(), Error> {
    let first = response_json(server.list_api("/", &[("limit", "1"), ("sort", "name")])?)?;
    let cursor = first["next_cursor"]
        .as_str()
        .ok_or("Expected a continuation cursor")?;

    let response = server.list_api("/", &[("limit", "1"), ("sort", "size"), ("cursor", cursor)])?;
    assert_eq!(response.status(), 400);
    assert_eq!(response.text()?, "Invalid list cursor");
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
    assert_eq!(response.status(), 400);
    assert_eq!(response.text()?, "Search query is too long");
    Ok(())
}

/// Reproducible local performance probe. It is ignored by the normal suite
/// because creating 100,000 real directory entries is intentionally costly.
#[rstest]
#[ignore = "manual large-directory benchmark"]
fn benchmark_one_hundred_thousand_entries(server: TestServer) -> Result<(), Error> {
    const CREATED: usize = 100_000;
    for index in 0..CREATED {
        std::fs::write(server.path().join(format!("bench-{index:06}")), [])?;
    }

    let started = Instant::now();
    let response = server.list_api("/", &[("limit", "500")])?;
    assert_eq!(response.status(), 200);
    let data = response_json(response)?;
    assert_eq!(data["paths"].as_array().map(Vec::len), Some(500));
    assert!(data["next_cursor"].is_string());
    eprintln!(
        "100000-entry first page completed in {:?}",
        started.elapsed()
    );
    Ok(())
}
