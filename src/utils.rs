use anyhow::{Result, anyhow};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use std::{borrow::Cow, path::Path};

const URI_COMPONENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

pub fn encode_uri(v: &str) -> String {
    v.split('/')
        .map(|part| utf8_percent_encode(part, URI_COMPONENT_ENCODE_SET).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn decode_uri(v: &str) -> Option<Cow<'_, str>> {
    percent_encoding::percent_decode(v.as_bytes())
        .decode_utf8()
        .ok()
}

pub fn get_file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
}

pub fn try_get_file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| anyhow!("Failed to get file name of `{}`", path.display()))
}

pub fn glob(pattern: &str, target: &str) -> bool {
    let pat = match ::glob::Pattern::new(pattern) {
        Ok(pat) => pat,
        Err(_) => return false,
    };
    pat.matches(target)
}

pub fn parse_range(range: &str, size: u64) -> Option<(u64, u64)> {
    let (unit, range) = range.split_once('=')?;
    if unit != "bytes" {
        return None;
    }
    if range.contains(',') {
        return None;
    }
    let (start, end) = range.trim().split_once('-')?;
    if start.is_empty() {
        let length = end.parse::<u64>().ok()?;
        if length == 0 || size == 0 {
            return None;
        }
        let length = length.min(size);
        return Some((size - length, size - 1));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= size {
        return None;
    }
    if end.is_empty() {
        return Some((start, size - 1));
    }
    let end = end.parse::<u64>().ok()?;
    if start > end {
        return None;
    }
    Some((start, end.min(size - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_key() {
        assert!(glob("", ""));
        assert!(glob(".*", ".git"));
        assert!(glob("abc", "abc"));
        assert!(glob("a*c", "abc"));
        assert!(glob("a?c", "abc"));
        assert!(glob("a*c", "abbc"));
        assert!(glob("*c", "abc"));
        assert!(glob("a*", "abc"));
        assert!(glob("?c", "bc"));
        assert!(glob("a?", "ab"));
        assert!(!glob("abc", "adc"));
        assert!(!glob("abc", "abcd"));
        assert!(!glob("a?c", "abbc"));
        assert!(!glob("*.log", "log"));
        assert!(glob("*.abc-cba", "xyz.abc-cba"));
        assert!(glob("*.abc-cba", "123.xyz.abc-cba"));
        assert!(glob("*.log", ".log"));
        assert!(glob("*.log", "a.log"));
        assert!(glob("*/", "abc/"));
        assert!(!glob("*/", "abc"));
    }

    #[test]
    fn test_parse_range() {
        assert_eq!(parse_range("bytes=0-499", 500), Some((0, 499)));
        assert_eq!(parse_range("bytes=0-", 500), Some((0, 499)));
        assert_eq!(parse_range("bytes=299-", 500), Some((299, 499)));
        assert_eq!(parse_range("bytes=-500", 500), Some((0, 499)));
        assert_eq!(parse_range("bytes=-300", 500), Some((200, 499)));
        assert_eq!(parse_range("bytes=-501", 500), Some((0, 499)));
        assert_eq!(parse_range("bytes=0-500", 500), Some((0, 499)));
        assert_eq!(parse_range("bytes=499-999", 500), Some((499, 499)));
        assert_eq!(parse_range("bytes=0-199, 100-399", 500), None);
        assert_eq!(parse_range("bytes=500-", 500), None);
        assert_eq!(parse_range("bytes=-0", 500), None);
        assert_eq!(parse_range("bytes=-1", 0), None);
        assert_eq!(parse_range("bytes=0-0", 0), None);
        assert_eq!(parse_range("bytes=0-199,", 500), None);
        assert_eq!(parse_range("bytes=0-199, 500-", 500), None);
    }
}
