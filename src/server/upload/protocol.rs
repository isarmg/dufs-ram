use super::super::{
    Response, path_coordinator::PathLease, protocol::UploadPublicState, router::MutationProgress,
};

use crate::utils::{decode_hex_to_slice, encode_hex};
use anyhow::{Result, anyhow};
use headers::HeaderMap;
use hyper::header::HeaderValue;
use serde::{Serialize, Serializer};
use std::fmt;
use tokio::time::Instant;
use uuid::Uuid;

pub(super) const RESUMABLE_UPLOAD_MIN_SIZE: u64 = 20 * 1024 * 1024;
pub(super) const UPLOAD_ID_HEADER: &str = "x-dufs-upload-id";
pub(super) const UPLOAD_LENGTH_HEADER: &str = "x-dufs-upload-length";
pub(super) const UPLOAD_OFFSET_HEADER: &str = "x-dufs-upload-offset";
pub(super) const UPLOAD_OVERWRITE_HEADER: &str = "x-dufs-upload-overwrite";
pub(in crate::server) const TARGET_REVISION_HEADER: &str = "x-dufs-target-revision";
pub(super) const TARGET_REPLACEABLE_HEADER: &str = "x-dufs-target-replaceable";
const OPERATION_STATE_HEADER: &str = "x-dufs-operation-state";

pub(in crate::server) struct UploadOptions {
    pub(in crate::server) owner: String,
    pub(in crate::server) mode: UploadMode,
    pub(in crate::server) upload_id: Uuid,
    pub(in crate::server) upload_length: u64,
    pub(in crate::server) overwrite: UploadOverwritePolicy,
    pub(in crate::server) deadline: Instant,
    pub(in crate::server) path_lease: PathLease,
    pub(in crate::server) mutation: MutationProgress,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::server) struct TargetRevision([u8; 32]);

impl TargetRevision {
    pub(in crate::server) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(in crate::server) const fn into_bytes(self) -> [u8; 32] {
        self.0
    }

    pub(in crate::server) fn encode(self) -> String {
        encode_hex(self.0)
    }

    pub(in crate::server) fn parse(value: &str) -> Option<Self> {
        let mut revision = [0_u8; 32];
        (value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && decode_hex_to_slice(value, &mut revision))
        .then_some(Self(revision))
    }
}

impl Serialize for TargetRevision {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.encode())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::server) enum UploadOverwritePolicy {
    NoReplace,
    IfUnchanged(TargetRevision),
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::server) enum UploadOverwriteParseError {
    UploadOverwrite(String),
    TargetRevision(String),
}

impl fmt::Display for UploadOverwriteParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::UploadOverwrite(detail) | Self::TargetRevision(detail) => detail,
        };
        formatter.write_str(detail)
    }
}

impl std::error::Error for UploadOverwriteParseError {}

impl UploadOverwritePolicy {
    pub(super) const fn revision(self) -> Option<TargetRevision> {
        match self {
            Self::NoReplace => None,
            Self::IfUnchanged(revision) => Some(revision),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum UploadMode {
    Fresh,
    Resume { offset: u64 },
}

impl UploadMode {
    pub(in crate::server) const fn offset(self) -> Option<u64> {
        match self {
            Self::Fresh => None,
            Self::Resume { offset } => Some(offset),
        }
    }

    pub(in crate::server) const fn is_resume(self) -> bool {
        matches!(self, Self::Resume { .. })
    }
}

pub(in crate::server) fn apply_upload_record_headers(
    res: &mut Response,
    upload_id: Uuid,
    upload_length: Option<u64>,
    upload_offset: Option<u64>,
    state: UploadPublicState,
) -> Result<()> {
    res.headers_mut().insert(
        UPLOAD_ID_HEADER,
        HeaderValue::from_str(&upload_id.to_string())?,
    );
    res.headers_mut().insert(
        OPERATION_STATE_HEADER,
        HeaderValue::from_static(state.wire_name()),
    );
    if let Some(upload_length) = upload_length {
        res.headers_mut().insert(
            UPLOAD_LENGTH_HEADER,
            HeaderValue::from_str(&upload_length.to_string())?,
        );
    }
    if let Some(upload_offset) = upload_offset {
        res.headers_mut().insert(
            UPLOAD_OFFSET_HEADER,
            HeaderValue::from_str(&upload_offset.to_string())?,
        );
    }
    Ok(())
}

fn unique_upload_header<'a>(
    headers: &'a HeaderMap<HeaderValue>,
    name: &'static str,
) -> Result<Option<&'a HeaderValue>> {
    let values = headers.get_all(name);
    let mut values = values.iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(anyhow!("Duplicate {name} header"));
    }
    Ok(first)
}

pub(in crate::server) fn parse_upload_id(headers: &HeaderMap<HeaderValue>) -> Result<Option<Uuid>> {
    let Some(value) = unique_upload_header(headers, UPLOAD_ID_HEADER)? else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| anyhow!("Invalid {UPLOAD_ID_HEADER} header"))?;
    if value.len() != 36
        || !value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
    {
        return Err(anyhow!(
            "Invalid {UPLOAD_ID_HEADER} header: expected a lowercase canonical UUID"
        ));
    }
    Uuid::parse_str(value)
        .map(Some)
        .map_err(|_| anyhow!("Invalid {UPLOAD_ID_HEADER} header"))
}

fn parse_canonical_u64(value: &HeaderValue, name: &'static str) -> Result<u64> {
    let value = value
        .to_str()
        .map_err(|_| anyhow!("Invalid {name} header"))?;
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(anyhow!(
            "Invalid {name} header: expected canonical unsigned decimal digits"
        ));
    }
    value
        .parse::<u64>()
        .map_err(|_| anyhow!("Invalid {name} header"))
}

pub(in crate::server) fn parse_upload_length(
    headers: &HeaderMap<HeaderValue>,
) -> Result<Option<u64>> {
    let Some(value) = unique_upload_header(headers, UPLOAD_LENGTH_HEADER)? else {
        return Ok(None);
    };
    parse_canonical_u64(value, UPLOAD_LENGTH_HEADER).map(Some)
}

pub(in crate::server) fn parse_upload_offset(
    headers: &HeaderMap<HeaderValue>,
) -> Result<Option<u64>> {
    let Some(value) = unique_upload_header(headers, UPLOAD_OFFSET_HEADER)? else {
        return Ok(None);
    };
    parse_canonical_u64(value, UPLOAD_OFFSET_HEADER).map(Some)
}

pub(in crate::server) fn parse_upload_overwrite(
    headers: &HeaderMap<HeaderValue>,
) -> std::result::Result<UploadOverwritePolicy, UploadOverwriteParseError> {
    let overwrite = unique_upload_header(headers, UPLOAD_OVERWRITE_HEADER)
        .map_err(|error| UploadOverwriteParseError::UploadOverwrite(error.to_string()))?;
    let revision = unique_upload_header(headers, TARGET_REVISION_HEADER)
        .map_err(|error| UploadOverwriteParseError::TargetRevision(error.to_string()))?;
    match overwrite.map(HeaderValue::as_bytes) {
        None | Some(b"false") => {
            if revision.is_some() {
                return Err(UploadOverwriteParseError::TargetRevision(format!(
                    "The {TARGET_REVISION_HEADER} header requires {UPLOAD_OVERWRITE_HEADER}: true"
                )));
            }
            Ok(UploadOverwritePolicy::NoReplace)
        }
        Some(b"true") => {
            let revision = revision.ok_or_else(|| {
                UploadOverwriteParseError::TargetRevision(format!(
                    "The {TARGET_REVISION_HEADER} header is required when {UPLOAD_OVERWRITE_HEADER} is true"
                ))
            })?;
            let revision = revision
                .to_str()
                .ok()
                .and_then(TargetRevision::parse)
                .ok_or_else(|| {
                    UploadOverwriteParseError::TargetRevision(format!(
                        "Invalid {TARGET_REVISION_HEADER} header: expected 64 lowercase hexadecimal characters"
                    ))
                })?;
            Ok(UploadOverwritePolicy::IfUnchanged(revision))
        }
        Some(_) => Err(UploadOverwriteParseError::UploadOverwrite(format!(
            "Invalid {UPLOAD_OVERWRITE_HEADER} header: expected true or false"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_upload_protocol_headers_are_rejected() {
        let mut headers = HeaderMap::new();
        headers.append(
            UPLOAD_ID_HEADER,
            HeaderValue::from_static("00112233-4455-6677-8899-aabbccddeeff"),
        );
        headers.append(
            UPLOAD_ID_HEADER,
            HeaderValue::from_static("ffeeddcc-bbaa-9988-7766-554433221100"),
        );
        assert!(
            parse_upload_id(&headers)
                .unwrap_err()
                .to_string()
                .contains("Duplicate")
        );

        let mut headers = HeaderMap::new();
        headers.append(UPLOAD_LENGTH_HEADER, HeaderValue::from_static("1"));
        headers.append(UPLOAD_LENGTH_HEADER, HeaderValue::from_static("2"));
        assert!(
            parse_upload_length(&headers)
                .unwrap_err()
                .to_string()
                .contains("Duplicate")
        );

        let mut headers = HeaderMap::new();
        headers.append(UPLOAD_OFFSET_HEADER, HeaderValue::from_static("0"));
        headers.append(UPLOAD_OFFSET_HEADER, HeaderValue::from_static("1"));
        assert!(
            parse_upload_offset(&headers)
                .unwrap_err()
                .to_string()
                .contains("Duplicate")
        );
    }

    #[test]
    fn upload_protocol_values_require_canonical_wire_representations() {
        for invalid in [
            "00112233445566778899aabbccddeeff",
            "00112233-4455-6677-8899-AABBCCDDEEFF",
            "{00112233-4455-6677-8899-aabbccddeeff}",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(UPLOAD_ID_HEADER, HeaderValue::from_str(invalid).unwrap());
            assert!(parse_upload_id(&headers).is_err(), "accepted {invalid}");
        }

        for (name, parse) in [
            (
                UPLOAD_LENGTH_HEADER,
                parse_upload_length as fn(&HeaderMap<HeaderValue>) -> Result<Option<u64>>,
            ),
            (UPLOAD_OFFSET_HEADER, parse_upload_offset),
        ] {
            for invalid in ["", "00", "01", "+1", " 1", "1 ", "-1"] {
                let mut headers = HeaderMap::new();
                headers.insert(name, HeaderValue::from_str(invalid).unwrap());
                assert!(parse(&headers).is_err(), "accepted {name}: {invalid:?}");
            }
            let mut headers = HeaderMap::new();
            headers.insert(name, HeaderValue::from_static("0"));
            assert_eq!(parse(&headers).unwrap(), Some(0));
        }
    }

    #[test]
    fn upload_record_headers_use_the_typed_wire_state() {
        let mut response = Response::default();
        let upload_id = Uuid::new_v4();
        apply_upload_record_headers(
            &mut response,
            upload_id,
            Some(9),
            Some(3),
            UploadPublicState::Running,
        )
        .unwrap();

        assert_eq!(response.headers()[UPLOAD_ID_HEADER], upload_id.to_string());
        assert_eq!(response.headers()[UPLOAD_LENGTH_HEADER], "9");
        assert_eq!(response.headers()[UPLOAD_OFFSET_HEADER], "3");
        assert_eq!(response.headers()[OPERATION_STATE_HEADER], "running");
    }

    #[test]
    fn overwrite_policy_requires_an_explicit_canonical_revision() {
        let headers = HeaderMap::new();
        assert_eq!(
            parse_upload_overwrite(&headers).unwrap(),
            UploadOverwritePolicy::NoReplace
        );

        let mut headers = HeaderMap::new();
        headers.insert(UPLOAD_OVERWRITE_HEADER, HeaderValue::from_static("true"));
        assert!(matches!(
            parse_upload_overwrite(&headers),
            Err(UploadOverwriteParseError::TargetRevision(_))
        ));

        headers.insert(
            TARGET_REVISION_HEADER,
            HeaderValue::from_static(
                "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
            ),
        );
        assert!(matches!(
            parse_upload_overwrite(&headers).unwrap(),
            UploadOverwritePolicy::IfUnchanged(_)
        ));

        headers.insert(
            TARGET_REVISION_HEADER,
            HeaderValue::from_static(
                "00112233445566778899AABBccddeeff00112233445566778899aabbccddeeff",
            ),
        );
        assert!(matches!(
            parse_upload_overwrite(&headers),
            Err(UploadOverwriteParseError::TargetRevision(_))
        ));

        headers.insert(UPLOAD_OVERWRITE_HEADER, HeaderValue::from_static("false"));
        assert!(
            matches!(
                parse_upload_overwrite(&headers),
                Err(UploadOverwriteParseError::TargetRevision(_))
            ),
            "a revision must never be silently ignored in no-replace mode"
        );

        headers.remove(TARGET_REVISION_HEADER);
        headers.insert(UPLOAD_OVERWRITE_HEADER, HeaderValue::from_static("TRUE"));
        assert!(matches!(
            parse_upload_overwrite(&headers),
            Err(UploadOverwriteParseError::UploadOverwrite(_))
        ));
    }
}
