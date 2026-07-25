mod fixtures;
mod utils;

use fixtures::{Error, TestServer, server};
use rstest::rstest;

#[rstest]
#[case(server(&[] as &[&str]), true)]
#[case(server(&["--hidden", ".git,index.html"]), false)]
fn hidden_get_dir(#[case] server: TestServer, #[case] exist: bool) -> Result<(), Error> {
    let resp = server.get(server.url())?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(paths.contains("dir1/"));
    assert_eq!(paths.contains(".git/"), exist);
    assert_eq!(paths.contains("index.html"), exist);
    Ok(())
}

#[rstest]
#[case(server(&[] as &[&str]), true)]
#[case(server(&["--hidden", "*.html"]), false)]
fn hidden_get_dir2(#[case] server: TestServer, #[case] exist: bool) -> Result<(), Error> {
    let resp = server.get(server.url())?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert!(paths.contains("dir1/"));
    assert_eq!(paths.contains("index.html"), exist);
    assert_eq!(paths.contains("test.html"), exist);
    Ok(())
}

#[rstest]
#[case(server(&[] as &[&str]), true)]
#[case(server(&["--hidden", ".git,test.html"]), false)]
fn hidden_search_dir(#[case] server: TestServer, #[case] exist: bool) -> Result<(), Error> {
    let resp = server.get(format!("{}?q={}", server.url(), "test.html"))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    for p in paths {
        assert_eq!(p.contains("test.html"), exist);
    }
    Ok(())
}

#[rstest]
#[case(server(&["--hidden", "hidden/"]), "dir4/", 1)]
#[case(server(&["--hidden", "hidden"]), "dir4/", 0)]
fn hidden_dir_only(
    #[case] server: TestServer,
    #[case] dir: &str,
    #[case] count: usize,
) -> Result<(), Error> {
    let resp = server.get(format!("{}{}", server.url(), dir))?;
    assert_eq!(resp.status(), 200);
    let paths = server.paths_from_page(resp)?;
    assert_eq!(paths.len(), count);
    Ok(())
}
