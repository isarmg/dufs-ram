use super::{
    Response, body_full,
    protocol::{OperationPublicState, UploadPublicState},
};

use anyhow::Error;
use hyper::{
    StatusCode,
    header::{CONTENT_TYPE, HeaderValue, RETRY_AFTER},
};
use serde::Serialize;
use std::{borrow::Cow, fmt};

/// A stable, machine-readable public error identifier.
///
/// Production code can only select one of the associated constants declared
/// in the catalog below. The tuple field and validating constructor remain
/// private so handlers cannot introduce an unreviewed wire value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub(super) struct ErrorCode(&'static str);

impl ErrorCode {
    const fn checked(value: &'static str) -> Self {
        let bytes = value.as_bytes();
        assert!(
            !bytes.is_empty() && bytes.len() <= 64,
            "problem code must contain between 1 and 64 bytes"
        );
        assert!(
            bytes[0] >= b'a' && bytes[0] <= b'z',
            "problem code must start with a lowercase ASCII letter"
        );
        let mut index = 1;
        while index < bytes.len() {
            let byte = bytes[index];
            assert!(
                (byte >= b'a' && byte <= b'z') || (byte >= b'0' && byte <= b'9') || byte == b'_',
                "problem code may contain only lowercase ASCII letters, digits, and underscores"
            );
            index += 1;
        }
        Self(value)
    }

    /// Test-only boundary for proving that invalid public codes fail closed.
    #[cfg(test)]
    pub(super) const fn from_test_value(value: &'static str) -> Self {
        Self::checked(value)
    }

    pub(super) const fn as_str(self) -> &'static str {
        self.0
    }
}

macro_rules! declare_error_codes {
    ($( $constant:ident => $wire_name:literal, )+) => {
        impl ErrorCode {
            $(pub(super) const $constant: Self = Self::checked($wire_name);)+

            #[cfg(test)]
            const ALL: &'static [Self] = &[$(Self::$constant),+];
        }
    };
}

declare_error_codes! {
    API_ENDPOINT_NOT_FOUND => "api_endpoint_not_found",
    AUTHENTICATION_REQUIRED => "authentication_required",
    CSRF_FAILED => "csrf_failed",
    DELETE_NOT_COMMITTED => "delete_not_committed",
    DELETE_STATE_CONFLICT => "delete_state_conflict",
    DELETE_TARGET_CHANGED => "delete_target_changed",
    DESTINATION_EXISTS => "destination_exists",
    DESTINATION_DIRECTORY_NOT_FOUND => "destination_directory_not_found",
    DESTINATION_NOT_DIRECTORY => "destination_not_directory",
    DIRECTORY_ACCESS_FORBIDDEN => "directory_access_forbidden",
    DIRECTORY_ARCHIVE_UNSUPPORTED => "directory_archive_unsupported",
    DIRECTORY_CHANGED => "directory_changed",
    DIRECTORY_DEPTH_LIMIT => "directory_depth_limit",
    DIRECTORY_ENTRY_LIMIT => "directory_entry_limit",
    DIRECTORY_INTO_ITSELF => "directory_into_itself",
    DIRECTORY_MEMORY_LIMIT => "directory_memory_limit",
    DIRECTORY_OPERATION_FAILED => "directory_operation_failed",
    DIRECTORY_OPERATION_LIMIT => "directory_operation_limit",
    DIRECTORY_OPERATION_TIMEOUT => "directory_operation_timeout",
    DIRECTORY_OVERWRITE_FORBIDDEN => "directory_overwrite_forbidden",
    DIRECTORY_SORT_LIMIT => "directory_sort_limit",
    DIRECTORY_SYMLINK_LOOP => "directory_symlink_loop",
    FILESYSTEM_TIMEOUT => "filesystem_timeout",
    INSUFFICIENT_STORAGE => "insufficient_storage",
    INTERNAL_ERROR => "internal_error",
    INVALID_DESTINATION_PATH => "invalid_destination_path",
    INVALID_JOB_ID => "invalid_job_id",
    INVALID_JSON => "invalid_json",
    INVALID_LIST_CURSOR => "invalid_list_cursor",
    INVALID_LIST_LIMIT => "invalid_list_limit",
    INVALID_LIST_PATH => "invalid_list_path",
    INVALID_MOVE_PATH => "invalid_move_path",
    INVALID_OPERATION_ID => "invalid_operation_id",
    INVALID_PATH => "invalid_path",
    INVALID_RENAME_NAME => "invalid_rename_name",
    INVALID_RENAME_PATH => "invalid_rename_path",
    INVALID_REQUEST => "invalid_request",
    INVALID_REQUEST_BODY => "invalid_request_body",
    INVALID_SOURCE_PATH => "invalid_source_path",
    INVALID_UPLOAD_ID => "invalid_upload_id",
    INVALID_UPLOAD_LENGTH => "invalid_upload_length",
    INVALID_UPLOAD_OFFSET => "invalid_upload_offset",
    INVALID_UPLOAD_OVERWRITE => "invalid_upload_overwrite",
    INVALID_TARGET_REVISION => "invalid_target_revision",
    JOB_NOT_FOUND => "job_not_found",
    JOB_STORE_UNAVAILABLE => "job_store_unavailable",
    LIST_CURSOR_UNAVAILABLE => "list_cursor_unavailable",
    LIST_PATH_NOT_DIRECTORY => "list_path_not_directory",
    LIST_PATH_NOT_FOUND => "list_path_not_found",
    LIST_SNAPSHOT_LIMIT => "list_snapshot_limit",
    METHOD_NOT_ALLOWED => "method_not_allowed",
    MOVE_STATE_CONFLICT => "move_state_conflict",
    MOVE_STATE_UNAVAILABLE => "move_state_unavailable",
    OPERATION_ID_CONFLICT => "operation_id_conflict",
    OPERATION_IN_PROGRESS => "operation_in_progress",
    OPERATION_NOT_COMMITTED => "operation_not_committed",
    OPERATION_REGISTRY_FULL => "operation_registry_full",
    OPERATION_RESULT_UNKNOWN => "operation_result_unknown",
    OPERATION_STORE_UNAVAILABLE => "operation_store_unavailable",
    OUTCOME_UNCERTAIN => "outcome_uncertain",
    PATH_EXISTS => "path_exists",
    PURGE_BACKLOG_FULL => "purge_backlog_full",
    PURGE_STATE_UNAVAILABLE => "purge_state_unavailable",
    REQUEST_BODY_TOO_LARGE => "request_body_too_large",
    REQUEST_CONFLICT => "request_conflict",
    REQUEST_FORBIDDEN => "request_forbidden",
    REQUEST_NOT_FOUND => "request_not_found",
    REQUEST_TIMEOUT => "request_timeout",
    RENAME_STATE_CONFLICT => "rename_state_conflict",
    RENAME_STATE_UNAVAILABLE => "rename_state_unavailable",
    ROOT_DELETE_FORBIDDEN => "root_delete_forbidden",
    SEARCH_QUERY_TOO_LONG => "search_query_too_long",
    SEARCH_RESULT_LIMIT => "search_result_limit",
    SERVER_STOPPING => "server_stopping",
    SOURCE_EQUALS_DESTINATION => "source_equals_destination",
    SOURCE_NOT_FOUND => "source_not_found",
    TARGET_NOT_FOUND => "target_not_found",
    UNSUPPORTED_FILENAME => "unsupported_filename",
    UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
    UPLOAD_BODY_EXCEEDS_REMAINING_LENGTH => "upload_body_exceeds_remaining_length",
    UPLOAD_CHECKPOINT_PERSIST_FAILED => "upload_checkpoint_persist_failed",
    UPLOAD_COMMITTED_LENGTH_MISMATCH => "upload_committed_length_mismatch",
    UPLOAD_CONCURRENCY_LIMIT => "upload_concurrency_limit",
    UPLOAD_ID_REJECTED => "upload_id_rejected",
    UPLOAD_IDLE_TIMEOUT => "upload_idle_timeout",
    UPLOAD_IN_PROGRESS => "upload_in_progress",
    UPLOAD_INSUFFICIENT_STORAGE => "upload_insufficient_storage",
    UPLOAD_LENGTH_CHANGED => "upload_length_changed",
    UPLOAD_LENGTH_MISMATCH => "upload_length_mismatch",
    UPLOAD_METADATA_PRESERVATION_REFUSED => "upload_metadata_preservation_refused",
    UPLOAD_NOT_PUBLISHED => "upload_not_published",
    UPLOAD_OFFSET_CHANGED => "upload_offset_changed",
    UPLOAD_OUTCOME_UNKNOWN => "upload_outcome_unknown",
    UPLOAD_PATH_WAIT_TIMEOUT => "upload_path_wait_timeout",
    UPLOAD_PRECOMMIT_FAILED => "upload_precommit_failed",
    UPLOAD_PUBLICATION_DURABILITY_UNKNOWN => "upload_publication_durability_unknown",
    UPLOAD_PUBLICATION_OUTCOME_UNKNOWN => "upload_publication_outcome_unknown",
    UPLOAD_RESULT_UNKNOWN => "upload_result_unknown",
    UPLOAD_SESSION_CONFLICT => "upload_session_conflict",
    UPLOAD_SESSION_NOT_FOUND => "upload_session_not_found",
    UPLOAD_SESSION_STORE_FULL => "upload_session_store_full",
    UPLOAD_SIZE_LIMIT_EXCEEDED => "upload_size_limit_exceeded",
    UPLOAD_STAGE_CONFLICT => "upload_stage_conflict",
    UPLOAD_STAGE_INVALID => "upload_stage_invalid",
    UPLOAD_STATE_CONFLICT => "upload_state_conflict",
    UPLOAD_STATE_UNAVAILABLE => "upload_state_unavailable",
    UPLOAD_TARGET_CHANGED => "upload_target_changed",
    UPLOAD_TARGET_INSPECTION_TIMEOUT => "upload_target_inspection_timeout",
    UPLOAD_TARGET_NOT_FOUND => "upload_target_not_found",
    UPLOAD_TERMINAL_RECORD_FAILED => "upload_terminal_record_failed",
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// A client action that is safe for this specific failure.
///
/// In particular, an HTTP 5xx status never implies retryability. Mutation
/// handlers must select an explicit recovery action after determining whether
/// the operation was rejected, is still running, or has an unknown outcome.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum RecoveryAdvice {
    #[default]
    None,
    Retry,
    RetryAfterSeconds(u64),
    RetryWithNewId,
    ResumeUpload,
    QueryJob,
    QueryUpload,
    RefreshTarget,
}

impl RecoveryAdvice {
    const fn wire_name(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Retry | Self::RetryAfterSeconds(_) => Some("retry"),
            Self::RetryWithNewId => Some("retry_with_new_id"),
            Self::ResumeUpload => Some("resume_upload"),
            Self::QueryJob => Some("query_job"),
            Self::QueryUpload => Some("query_upload"),
            Self::RefreshTarget => Some("refresh_target"),
        }
    }

    const fn retry_after_seconds(self) -> Option<u64> {
        match self {
            Self::RetryAfterSeconds(seconds) => Some(seconds),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OperationProblemContext {
    operation_id: String,
    state: OperationPublicState,
    http_status: Option<u16>,
}

impl OperationProblemContext {
    pub(super) fn new(
        operation_id: impl Into<String>,
        state: OperationPublicState,
        http_status: Option<u16>,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            state,
            http_status,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UploadProblemContext {
    upload_id: String,
    state: UploadPublicState,
    upload_length: Option<u64>,
    upload_offset: Option<u64>,
}

impl UploadProblemContext {
    pub(super) fn new(
        upload_id: impl Into<String>,
        state: UploadPublicState,
        upload_length: Option<u64>,
        upload_offset: Option<u64>,
    ) -> Self {
        Self {
            upload_id: upload_id.into(),
            state,
            upload_length,
            upload_offset,
        }
    }
}

/// Internal error metadata. `source` is diagnostic-only and is never included
/// in the public problem document.
#[derive(Debug)]
pub(super) struct ApiError {
    status: StatusCode,
    code: ErrorCode,
    detail: Cow<'static, str>,
    source: Option<Error>,
    recovery: RecoveryAdvice,
    operation: Option<OperationProblemContext>,
    upload: Option<UploadProblemContext>,
}

impl ApiError {
    pub(super) fn new(
        status: StatusCode,
        code: ErrorCode,
        detail: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
            source: None,
            recovery: RecoveryAdvice::None,
            operation: None,
            upload: None,
        }
    }

    #[cfg(test)]
    fn with_source(mut self, source: Error) -> Self {
        self.source = Some(source);
        self
    }

    pub(super) const fn with_recovery(mut self, recovery: RecoveryAdvice) -> Self {
        self.recovery = recovery;
        self
    }

    pub(super) fn with_operation(mut self, operation: OperationProblemContext) -> Self {
        self.operation = Some(operation);
        self.upload = None;
        self
    }

    pub(super) fn with_upload(mut self, upload: UploadProblemContext) -> Self {
        self.upload = Some(upload);
        self.operation = None;
        self
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(formatter, "{source:#}"),
            None => formatter.write_str(&self.detail),
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// RFC 9457-style public representation used by first-party API clients.
#[derive(Debug, Serialize)]
struct ProblemDetails<'a> {
    #[serde(rename = "type")]
    problem_type: String,
    title: &'static str,
    status: u16,
    detail: &'a str,
    code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_offset: Option<u64>,
}

impl<'a> From<&'a ApiError> for ProblemDetails<'a> {
    fn from(error: &'a ApiError) -> Self {
        let (
            operation_id,
            upload_id,
            state,
            upload_state,
            http_status,
            upload_length,
            upload_offset,
        ) = match (&error.operation, &error.upload) {
            (Some(operation), None) => (
                Some(operation.operation_id.as_str()),
                None,
                Some(operation.state.wire_name()),
                None,
                operation.http_status,
                None,
                None,
            ),
            (None, Some(upload)) => (
                None,
                Some(upload.upload_id.as_str()),
                None,
                Some(upload.state.wire_name()),
                None,
                upload.upload_length,
                upload.upload_offset,
            ),
            (None, None) => (None, None, None, None, None, None, None),
            (Some(_), Some(_)) => unreachable!("problem contexts are mutually exclusive"),
        };
        Self {
            problem_type: format!("urn:dufs:problem:{}", error.code),
            title: error.status.canonical_reason().unwrap_or("Request Failed"),
            status: error.status.as_u16(),
            detail: &error.detail,
            code: error.code,
            recovery: error.recovery.wire_name(),
            retry_after: error.recovery.retry_after_seconds(),
            operation_id,
            upload_id,
            state,
            upload_state,
            http_status,
            upload_length,
            upload_offset,
        }
    }
}

/// Render the only machine-readable error representation used by internal API
/// handlers.
pub(super) fn render_problem(res: &mut Response, error: &ApiError) -> anyhow::Result<()> {
    let output = serde_json::to_vec(&ProblemDetails::from(error))?;
    *res.status_mut() = error.status;
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    match error.recovery.retry_after_seconds() {
        Some(seconds) => {
            res.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_str(&seconds.to_string())?);
        }
        None => {
            res.headers_mut().remove(RETRY_AFTER);
        }
    }
    *res.body_mut() = body_full(output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;
    use serde_json::json;

    #[test]
    #[should_panic(expected = "problem code may contain only")]
    fn error_code_rejects_values_the_browser_cannot_consume() {
        let _ = ErrorCode::from_test_value("invalid-Code");
    }

    #[test]
    fn error_code_catalog_is_unique_and_serializes_exact_wire_names() {
        let mut seen = std::collections::BTreeSet::new();
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_test_value(code.as_str()), *code);
            assert!(seen.insert(code.as_str()), "duplicate code: {code}");
            assert_eq!(
                serde_json::to_string(code).unwrap(),
                format!("\"{}\"", code.as_str())
            );
        }
    }

    #[tokio::test]
    async fn renderer_keeps_diagnostics_private_and_emits_retry_metadata() {
        let mut response = Response::default();
        let error = ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::OPERATION_REGISTRY_FULL,
            "Operation registry is temporarily full",
        )
        .with_source(anyhow::anyhow!("private /srv/share diagnostic"))
        .with_recovery(RecoveryAdvice::RetryAfterSeconds(1))
        .with_operation(OperationProblemContext::new(
            "00112233-4455-6677-8899-aabbccddeeff",
            OperationPublicState::Rejected,
            None,
        ));

        render_problem(&mut response, &error).unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        assert_eq!(
            body,
            json!({
                "type": "urn:dufs:problem:operation_registry_full",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "Operation registry is temporarily full",
                "code": "operation_registry_full",
                "recovery": "retry",
                "retry_after": 1,
                "operation_id": "00112233-4455-6677-8899-aabbccddeeff",
                "state": "rejected"
            })
        );
        assert!(!body.to_string().contains("/srv/share"));
        assert!(error.to_string().contains("/srv/share"));
    }
}
