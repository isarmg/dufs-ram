mod fixtures;
mod utils;

use fixtures::{Error, TestServer, server};
use rstest::rstest;

#[rstest]
fn ls_dir_sort_by_name(server: TestServer) -> Result<(), Error> {
    let url = server.url();
    let resp = server.get(format!("{url}?sort=name&order=asc"))?;
    let paths1 = server.paths_from_page(resp)?;
    let resp = server.get(format!("{url}?sort=name&order=desc"))?;
    let mut paths2 = server.paths_from_page(resp)?;
    paths2.reverse();
    assert_eq!(paths1, paths2);
    Ok(())
}

#[rstest]
fn search_dir_sort_by_name(server: TestServer) -> Result<(), Error> {
    let url = server.url();
    let resp = server.get(format!("{url}?q=test.html&sort=name&order=asc"))?;
    let paths1 = server.paths_from_page(resp)?;
    let resp = server.get(format!("{url}?q=test.html&sort=name&order=desc"))?;
    let mut paths2 = server.paths_from_page(resp)?;
    paths2.reverse();
    assert_eq!(paths1, paths2);
    Ok(())
}
