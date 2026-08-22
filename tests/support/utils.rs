use base64::{Engine as _, engine::general_purpose::STANDARD};
use indexmap::IndexSet;
use serde_json::Value;

#[macro_export]
macro_rules! assert_resp_paths {
    ($server:ident, $resp:ident) => {
        assert_resp_paths!($server, $resp, self::fixtures::FILES)
    };
    ($server:ident, $resp:ident, $files:expr_2021) => {
        assert_eq!($resp.status(), 200);
        let paths = $server.paths_from_page($resp)?;
        assert!(!paths.is_empty());
        for file in $files {
            assert!(paths.contains(&file.to_string()));
        }
    };
}

#[allow(dead_code)]
pub fn retrieve_index_paths(content: &str) -> IndexSet<String> {
    let value = retrieve_json(content).unwrap();
    value
        .get("paths")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|v| {
            let name = v.get("name")?.as_str()?;
            let path_type = v.get("path_type")?.as_str()?;
            if path_type.ends_with("Dir") {
                Some(format!("{name}/"))
            } else {
                Some(name.to_owned())
            }
        })
        .collect()
}

#[allow(dead_code)]
pub fn encode_uri(v: &str) -> String {
    dufs::utils::encode_uri(v)
}

#[allow(dead_code)]
pub fn retrieve_json(content: &str) -> Option<Value> {
    let lines: Vec<&str> = content.lines().collect();
    let start_tag = "<template id=\"index-data\">";
    let end_tag = "</template>";

    let line = lines.iter().find(|v| v.contains(start_tag))?;

    let start_index = line.find(start_tag)?;
    let start_content_index = start_index + start_tag.len();

    let end_index = line[start_content_index..].find(end_tag)?;
    let end_content_index = start_content_index + end_index;

    let value = &line[start_content_index..end_content_index];
    let value = STANDARD.decode(value).ok()?;
    let value = serde_json::from_slice(&value).ok()?;

    Some(value)
}
