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

pub fn encode_hex(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

pub(crate) fn decode_hex_to_slice(input: &str, output: &mut [u8]) -> bool {
    if input.len() != output.len().saturating_mul(2) {
        return false;
    }
    for (pair, slot) in input.as_bytes().chunks_exact(2).zip(output) {
        let Some(high) = hex_nibble(pair[0]) else {
            return false;
        };
        let Some(low) = hex_nibble(pair[1]) else {
            return false;
        };
        *slot = (high << 4) | low;
    }
    true
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
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

    #[test]
    fn range_parser_property_never_returns_an_out_of_bounds_interval() {
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        for _ in 0..50_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let size = state % 4096;
            let first = state.rotate_left(19) % 8192;
            let second = state.rotate_left(41) % 8192;
            for value in [
                format!("bytes={first}-{second}"),
                format!("bytes={first}-"),
                format!("bytes=-{second}"),
            ] {
                if let Some((start, end)) = parse_range(&value, size) {
                    assert!(size > 0, "range returned for an empty resource: {value}");
                    assert!(start <= end, "inverted range for {value}");
                    assert!(end < size, "out-of-bounds range for {value} size={size}");
                }
            }
        }
    }

    #[test]
    fn uri_component_encoding_round_trips_unicode_and_reserved_bytes() {
        for value in [
            "",
            "plain",
            "space and #fragment?",
            "资料/子目录/file.txt",
            r"..\windows:name ",
            "\0\u{7f}\u{80}",
        ] {
            let encoded = encode_uri(value);
            assert_eq!(decode_uri(&encoded).as_deref(), Some(value));
        }
    }

    #[test]
    fn hexadecimal_codec_matches_lowercase_wire_format_and_rejects_invalid_input() {
        assert_eq!(encode_hex([0x00, 0x1f, 0xa5, 0xff]), "001fa5ff");
        let mut decoded = [0_u8; 4];
        assert!(decode_hex_to_slice("001FA5ff", &mut decoded));
        assert_eq!(decoded, [0x00, 0x1f, 0xa5, 0xff]);
        assert!(!decode_hex_to_slice("001fa5", &mut decoded));
        assert!(!decode_hex_to_slice("001fa5fg", &mut decoded));
    }
}
