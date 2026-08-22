use super::*;

#[rstest]
fn get_file(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}index.html", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/html");
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert!(resp.headers().contains_key("etag"));
    assert!(resp.headers().contains_key("last-modified"));
    assert!(resp.headers().contains_key("content-length"));
    assert_eq!(resp.text()?, "This is index.html");
    Ok(())
}

#[rstest]
fn head_file(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}index.html", server.url()))
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/html");
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert!(resp.headers().contains_key("content-disposition"));
    assert!(resp.headers().contains_key("etag"));
    assert!(resp.headers().contains_key("last-modified"));
    assert!(resp.headers().contains_key("content-length"));
    assert_eq!(resp.text()?, "");
    Ok(())
}

#[rstest]
fn get_file_404(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}404", server.url()))?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn get_file_emoji_path(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}{BIN_FILE}", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"download\"; filename*=UTF-8''%F0%9F%98%80.bin"
    );
    Ok(())
}

#[rstest]
fn get_file_newline_path(server: TestServer) -> Result<(), Error> {
    std::fs::write(server.path().join("file\n1.txt"), b"newline")?;
    let resp = server.get(format!("{}file%0A1.txt", server.url()))?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"download\"; filename*=UTF-8''file%201.txt"
    );
    Ok(())
}

#[rstest]
fn content_disposition_safely_encodes_special_filenames(server: TestServer) -> Result<(), Error> {
    let cases = [
        (
            "quote\"name.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''quote%22name.txt",
        ),
        (
            "back\\slash.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''back%5Cslash.txt",
        ),
        (
            "semi;colon.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''semi%3Bcolon.txt",
        ),
        (
            "percent%name.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''percent%25name.txt",
        ),
        (
            "空 格.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''%E7%A9%BA%20%E6%A0%BC.txt",
        ),
        (
            "tab\tname.txt",
            "attachment; filename=\"download\"; filename*=UTF-8''tab%20name.txt",
        ),
    ];

    for (filename, expected) in cases {
        std::fs::write(server.path().join(filename), b"contents")?;
        let resp = server.get(format!("{}{}", server.url(), utils::encode_uri(filename)))?;
        assert_eq!(resp.status(), 200, "unexpected status for {filename:?}");
        assert_eq!(
            resp.headers().get("content-disposition").unwrap(),
            expected,
            "unexpected Content-Disposition for {filename:?}",
        );
    }
    Ok(())
}

#[rstest]
fn unknown_file_query_still_returns_a_download(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(
            reqwest::Method::GET,
            format!("{}index.html?unused=1", server.url()),
        )
        .send()?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"download\"; filename*=UTF-8''index.html"
    );
    Ok(())
}

#[rstest]
fn head_file_404(server: TestServer) -> Result<(), Error> {
    let resp = server
        .request(reqwest::Method::HEAD, format!("{}404", server.url()))
        .send()?;
    assert_eq!(resp.status(), 404);
    Ok(())
}

#[rstest]
fn get_file_content_type(server: TestServer) -> Result<(), Error> {
    let resp = server.get(format!("{}content-types/bin.tar", server.url()))?;
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/x-tar"
    );
    let resp = server.get(format!("{}content-types/bin", server.url()))?;
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    let resp = server.get(format!("{}content-types/file-utf8.txt", server.url()))?;
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/plain");
    let resp = server.get(format!("{}content-types/file-gbk.txt", server.url()))?;
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/plain");
    let resp = server.get(format!("{}content-types/file", server.url()))?;
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    Ok(())
}
