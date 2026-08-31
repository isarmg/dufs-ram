use super::*;

#[rstest]
fn get_dir(server: TestServer) -> Result<(), Error> {
    let resp = server.get(server.url())?;
    assert!(resp.headers().contains_key("content-length"));
    assert_index_security_headers(resp.headers());
    assert_resp_paths!(server, resp);
    Ok(())
}

#[rstest]
fn head_dir(server: TestServer) -> Result<(), Error> {
    let resp = server.request(reqwest::Method::HEAD, server.url()).send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    assert!(!resp.headers().contains_key("content-length"));
    assert_index_security_headers(resp.headers());
    assert_eq!(resp.text()?, "");
    Ok(())
}

#[rstest]
fn get_missing_dir_shows_upload_target(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}404/", server.url()))?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn head_missing_dir_shows_upload_target(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}404/", server.url()))
        .send()?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

#[rstest]
fn nul_path_is_rejected_before_filesystem_dispatch(server: TestServer) -> Result<(), Error> {
    let response = server.get(format!("{}invalid%00name", server.url()))?;
    assert_eq!(response.status(), 400);
    assert_eq!(response.text()?, "Invalid Path");
    Ok(())
}

#[rstest]
fn method_not_allowed_responses_advertise_supported_methods(
    server: TestServer,
) -> Result<(), Error> {
    let response = server
        .request(reqwest::Method::OPTIONS, server.url().join("index.html")?)
        .send()?;
    assert_eq!(response.status(), 405);
    assert_eq!(
        response.headers().get("allow").unwrap(),
        "GET, HEAD, PUT, PATCH, DELETE"
    );

    let login = server
        .raw_request(reqwest::Method::PUT, server.url().join("__dufs__/login")?)
        .send()?;
    assert_eq!(login.status(), 405);
    assert_eq!(login.headers().get("allow").unwrap(), "GET");

    let login_api = server
        .raw_request(
            reqwest::Method::GET,
            server.url().join("api/v2/auth/login")?,
        )
        .send()?;
    assert_eq!(login_api.status(), 405);
    assert_eq!(login_api.headers().get("allow").unwrap(), "POST");
    Ok(())
}

#[rstest]
fn encoded_backslash_upload_is_preserved(server: TestServer) -> Result<(), Error> {
    let directory = server.path().join("upload-path-policy");
    std::fs::create_dir(&directory)?;
    let unsafe_path = directory.join("..\\escape.txt");
    let upload = with_new_upload_headers(
        server.request(
            reqwest::Method::PUT,
            format!("{}upload-path-policy/..%5Cescape.txt", server.url()),
        ),
        8,
    )
    .body(b"uploaded".to_vec())
    .send()?;
    assert_eq!(upload.status(), 201);
    assert_eq!(std::fs::read(&unsafe_path)?, b"uploaded");
    Ok(())
}

#[rstest]
fn dot_directory_and_log_file_are_visible_in_listing_and_search(
    server: TestServer,
) -> Result<(), Error> {
    std::fs::create_dir(server.path().join(".git"))?;
    std::fs::write(server.path().join(".git/HEAD"), b"ref: refs/heads/main\n")?;
    std::fs::write(server.path().join("activity.log"), b"ordinary log file\n")?;

    let listing = server.paths_from_page(server.get(server.url())?)?;
    assert!(listing.contains(".git/"));
    assert!(listing.contains("activity.log"));

    for (query, expected) in [(".git", ".git/"), (".log", "activity.log")] {
        let response = server.get(format!("{}?q={query}", server.url()))?;
        let paths = server.paths_from_page(response)?;
        assert!(
            paths.contains(expected),
            "search query {query:?} did not return {expected:?}: {paths:?}"
        );
    }
    Ok(())
}

#[rstest]
#[case("unused=1")]
#[case("zip")]
#[case("zip=1")]
fn unknown_directory_query_is_ignored(
    server: TestServer,
    #[case] query: &str,
) -> Result<(), Error> {
    let resp = server.get(format!("{}?{query}", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    let paths = server.paths_from_page(resp)?;
    assert!(paths.contains("index.html"));
    Ok(())
}

#[rstest]
fn non_utf8_names_are_rejected_without_partial_browser_results(
    server: TestServer,
) -> Result<(), Error> {
    let directory = server.path().join("utf8-policy");
    std::fs::create_dir(&directory)?;
    std::fs::write(directory.join("visible.txt"), b"visible")?;
    let invalid_name = OsString::from_vec(b"invalid-\xff.txt".to_vec());
    let invalid_path = directory.join(&invalid_name);
    std::fs::write(&invalid_path, b"must not become an empty browser path")?;

    for response in [
        server.list_api("/utf8-policy", &[])?,
        server.list_api("/utf8-policy", &[("q", "visible")])?,
    ] {
        assert_eq!(response.status(), 409);
        assert_problem_code(response, "unsupported_filename")?;
    }

    assert!(directory.is_dir());
    assert!(invalid_path.is_file());
    assert_eq!(std::fs::read(directory.join("visible.txt"))?, b"visible");
    Ok(())
}

#[rstest]
fn recursive_walk_errors_do_not_return_partial_search_success(
    server: TestServer,
) -> Result<(), Error> {
    let directory = server.path().join("walk-error");
    std::fs::create_dir(&directory)?;
    std::fs::write(directory.join("partial-match.txt"), b"partial")?;
    std::os::unix::fs::symlink(".", directory.join("loop"))?;

    let search = server.list_api("/walk-error", &[("q", "partial")])?;
    assert_eq!(search.status(), 409);
    assert_problem_code(search, "directory_symlink_loop")?;
    Ok(())
}

#[rstest]
fn recursive_walk_budget_bounds_one_large_directory_before_full_collection(
    #[with(&["--max-search-entries", "4"])] server: TestServer,
) -> Result<(), Error> {
    const CREATED: usize = 128;

    let directory = server.path().join("bounded-walk");
    std::fs::create_dir(&directory)?;
    for index in 0..CREATED {
        std::fs::write(
            directory.join(format!("entry-{index:03}.txt")),
            index.to_string(),
        )?;
    }

    let search = server.list_api("/bounded-walk", &[("q", "entry")])?;
    assert_eq!(search.status(), 413);
    assert_problem_code(search, "directory_entry_limit")?;
    Ok(())
}

#[rstest]
fn get_dir_search(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q={}", server.url(), "test.html"))?;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("content-length"));
    let paths = server.paths_from_page(resp)?;
    assert!(!paths.is_empty());
    for p in paths {
        assert!(p.contains("test.html"));
    }
    Ok(())
}

#[rstest]
fn get_dir_search2(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q={BIN_FILE}", server.url()))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(!paths.is_empty());
    for p in paths {
        assert!(p.contains(BIN_FILE));
    }
    Ok(())
}

#[rstest]
fn get_dir_search3(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q={}", server.url(), "test.html"))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(paths.iter().any(|path| path.contains("test.html")));
    Ok(())
}

#[rstest]
fn get_dir_search4(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}dir1?q=dir1", server.url()))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(paths.is_empty());
    Ok(())
}

#[rstest]
fn head_dir_search(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(
            reqwest::Method::HEAD,
            format!("{}?q={}", server.url(), "test.html"),
        )
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    assert!(!resp.headers().contains_key("content-length"));
    assert_index_security_headers(resp.headers());
    assert_eq!(resp.text()?, "");
    Ok(())
}

#[rstest]
fn empty_search(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}?q=", server.url()))?;
    assert_resp_paths!(server, resp);
    Ok(())
}
