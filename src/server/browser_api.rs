use super::{
    Request, Response, Server, body_full,
    identity::OwnerId,
    operation_registry::{
        BeginOperation, OperationFingerprint, OperationGuard, OperationOutcome,
        TrackedOperationError, apply_conflict, apply_invalid_id, apply_operation_outcome,
        apply_registry_full, apply_registry_unavailable, apply_running,
        parse_canonical_operation_id, parse_operation_id, set_operation_headers,
    },
    path_policy::BROWSER_COMPONENT_BYTES_LIMIT,
    problem::{ApiError, ErrorCode, OperationProblemContext, RecoveryAdvice, render_problem},
    protocol::OperationPublicState,
    rooted_fs::{CheckedRelocationOutcome, ReplacementTargetIdentity, RootedFs},
    router::MutationProgress,
    status_no_content,
    upload::{TARGET_REVISION_HEADER, TargetRevision, target_revision},
};
use crate::http_utils::request_content_type_is;

use anyhow::Result;
use bytes::Bytes;
use headers::{ContentLength, ContentType, HeaderMapExt};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::{
    StatusCode,
    header::{CACHE_CONTROL, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(super) const BROWSER_API_PREFIX: &str = "__dufs__/api/";
const MKDIR_API_PATH: &str = "__dufs__/api/mkdir";
const MOVE_API_PATH: &str = "__dufs__/api/move";
const RENAME_API_PATH: &str = "__dufs__/api/rename";
const UPLOAD_PREFLIGHT_API_PATH: &str = "__dufs__/api/upload/preflight";
const UPLOAD_DISCARD_API_PATH: &str = "__dufs__/api/upload/discard";
const API_BODY_LIMIT: usize = 16 * 1024;
const UPLOAD_PREFLIGHT_BODY_LIMIT: usize = 2 * 1024 * 1024;
const UPLOAD_PREFLIGHT_PATH_LIMIT: usize = 512;
const UPLOAD_PREFLIGHT_PATH_BYTES_LIMIT: usize = 256 * 1024;
pub(super) const UPLOAD_PREFLIGHT_CONCURRENCY: usize = 4;
pub(in crate::server) const SOURCE_REVISION_HEADER: &str = "x-dufs-source-revision";
type TrackedOperation = Option<(Uuid, OperationGuard)>;

pub(super) fn is_tracked_browser_mutation(path: &str) -> bool {
    matches!(path, MKDIR_API_PATH | MOVE_API_PATH | RENAME_API_PATH)
}

pub(super) fn is_browser_api_endpoint(path: &str) -> bool {
    is_tracked_browser_mutation(path)
        || matches!(path, UPLOAD_PREFLIGHT_API_PATH | UPLOAD_DISCARD_API_PATH)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirRequest {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveRequest {
    source: String,
    directory: String,
    #[serde(default)]
    source_revision: Option<String>,
    #[serde(default)]
    destination_revision: Option<String>,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameRequest {
    source: String,
    name: String,
    #[serde(default)]
    source_revision: Option<String>,
    #[serde(default)]
    destination_revision: Option<String>,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadPreflightRequest {
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadDiscardRequest {
    path: String,
    upload_id: String,
}

#[derive(Debug, Serialize)]
struct UploadPreflightResponse {
    targets: Vec<UploadPreflightTarget>,
}

#[derive(Debug, Serialize)]
struct UploadPreflightTarget {
    path: String,
    exists: bool,
    revision: Option<String>,
    replaceable: bool,
}

fn status_api_error(
    res: &mut Response,
    status: StatusCode,
    code: ErrorCode,
    message: &'static str,
    operation_id: Option<Uuid>,
) -> Result<()> {
    let mut error = ApiError::new(status, code, message);
    if let Some(operation_id) = operation_id {
        set_operation_headers(res, operation_id, OperationPublicState::Rejected);
        error = error.with_operation(OperationProblemContext::new(
            operation_id.hyphenated().to_string(),
            OperationPublicState::Rejected,
            None,
        ));
    }
    render_problem(res, &error)
}

fn request_has_json_content_type(req: &Request) -> bool {
    request_content_type_is(req.headers(), "application/json")
}

async fn collect_untracked_api_body(
    req: Request,
    limit: usize,
    res: &mut Response,
) -> Result<Option<Bytes>> {
    if !request_has_json_content_type(&req) {
        status_api_error(
            res,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
            None,
        )?;
        return Ok(None);
    }
    match Limited::new(req.into_body(), limit).collect().await {
        Ok(body) => Ok(Some(body.to_bytes())),
        Err(error) => {
            if error.downcast_ref::<LengthLimitError>().is_some() {
                status_api_error(
                    res,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    ErrorCode::REQUEST_BODY_TOO_LARGE,
                    "Request body too large",
                    None,
                )?;
            } else {
                status_api_error(
                    res,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::INVALID_REQUEST_BODY,
                    "Invalid request body",
                    None,
                )?;
            }
            Ok(None)
        }
    }
}

fn write_json_response<T: Serialize>(res: &mut Response, value: &T) -> Result<()> {
    let output = serde_json::to_vec(value)?;
    res.headers_mut()
        .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
    res.headers_mut()
        .typed_insert(ContentLength(output.len() as u64));
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    *res.body_mut() = body_full(output);
    Ok(())
}

fn split_operation(operation: TrackedOperation) -> (Option<Uuid>, Option<OperationGuard>) {
    match operation {
        Some((operation_id, operation)) => (Some(operation_id), Some(operation)),
        None => (None, None),
    }
}

async fn finish_api_error(
    operation: TrackedOperation,
    res: &mut Response,
    status: StatusCode,
    error: TrackedOperationError,
) -> Result<()> {
    let outcome = OperationOutcome::failure(status, error);
    match operation {
        Some((operation_id, operation)) => {
            operation.complete(outcome).await?;
            apply_operation_outcome(res, operation_id, outcome, false)
        }
        None => status_api_error(res, status, error.code(), error.detail(), None),
    }
}

fn render_completed_error(
    operation_id: Option<Uuid>,
    res: &mut Response,
    status: StatusCode,
    error: TrackedOperationError,
) -> Result<()> {
    match operation_id {
        Some(operation_id) => apply_operation_outcome(
            res,
            operation_id,
            OperationOutcome::failure(status, error),
            false,
        ),
        None => status_api_error(res, status, error.code(), error.detail(), None),
    }
}

fn render_completed_success(
    operation_id: Option<Uuid>,
    res: &mut Response,
    status: StatusCode,
) -> Result<()> {
    match operation_id {
        Some(operation_id) => {
            apply_operation_outcome(res, operation_id, OperationOutcome::success(status), false)
        }
        None => {
            if status == StatusCode::NO_CONTENT {
                status_no_content(res);
            } else {
                *res.status_mut() = status;
            }
            Ok(())
        }
    }
}

fn render_relocation_state_unavailable(
    kind: RelocationKind,
    operation_id: Option<Uuid>,
    res: &mut Response,
) -> Result<()> {
    let mut error = ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        kind.state_unavailable_code(),
        kind.state_unavailable_detail(),
    )
    .with_recovery(RecoveryAdvice::RetryAfterSeconds(1));
    if let Some(operation_id) = operation_id {
        set_operation_headers(res, operation_id, OperationPublicState::Rejected);
        error = error.with_operation(OperationProblemContext::new(
            operation_id.hyphenated().to_string(),
            OperationPublicState::Rejected,
            None,
        ));
    }
    render_problem(res, &error)
}

fn render_mkdir_state_unavailable(operation_id: Option<Uuid>, res: &mut Response) -> Result<()> {
    let mut error = ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::MKDIR_STATE_UNAVAILABLE,
        "Directory safety state is temporarily unavailable",
    )
    .with_recovery(RecoveryAdvice::RetryAfterSeconds(1));
    if let Some(operation_id) = operation_id {
        set_operation_headers(res, operation_id, OperationPublicState::Rejected);
        error = error.with_operation(OperationProblemContext::new(
            operation_id.hyphenated().to_string(),
            OperationPublicState::Rejected,
            None,
        ));
    }
    render_problem(res, &error)
}

impl Server {
    pub(super) async fn handle_browser_api(
        &self,
        endpoint: &str,
        owner: &str,
        req: Request,
        mutation: MutationProgress,
        res: &mut Response,
    ) -> Result<()> {
        if endpoint == UPLOAD_PREFLIGHT_API_PATH {
            return self.handle_upload_preflight(owner, req, res).await;
        }
        if endpoint == UPLOAD_DISCARD_API_PATH {
            return self.handle_upload_discard(owner, req, res).await;
        }
        if !matches!(endpoint, MKDIR_API_PATH | MOVE_API_PATH | RENAME_API_PATH) {
            status_api_error(
                res,
                StatusCode::NOT_FOUND,
                ErrorCode::API_ENDPOINT_NOT_FOUND,
                "API endpoint not found",
                None,
            )?;
            return Ok(());
        }
        let operation_id = match parse_operation_id(req.headers()) {
            Ok(Some(operation_id)) => operation_id,
            Ok(None) => {
                apply_invalid_id(res, "The x-dufs-operation-id header is required")?;
                return Ok(());
            }
            Err(message) => {
                apply_invalid_id(res, message)?;
                return Ok(());
            }
        };
        let is_json = request_content_type_is(req.headers(), "application/json");
        if !is_json {
            status_api_error(
                res,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
                Some(operation_id),
            )?;
            return Ok(());
        }

        let body = match Limited::new(req.into_body(), API_BODY_LIMIT)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(err) => {
                if err.downcast_ref::<LengthLimitError>().is_some() {
                    status_api_error(
                        res,
                        StatusCode::PAYLOAD_TOO_LARGE,
                        ErrorCode::REQUEST_BODY_TOO_LARGE,
                        "Request body too large",
                        Some(operation_id),
                    )?;
                } else {
                    status_api_error(
                        res,
                        StatusCode::BAD_REQUEST,
                        ErrorCode::INVALID_REQUEST_BODY,
                        "Invalid request body",
                        Some(operation_id),
                    )?;
                }
                return Ok(());
            }
        };
        let fingerprint = OperationFingerprint::new(&[b"POST", endpoint.as_bytes(), &body]);
        let operation = match self
            .state
            .operation_registry
            .begin(owner, operation_id, fingerprint)
            .await?
        {
            BeginOperation::Started(operation) => {
                mutation.mark_reserved();
                Some((operation_id, operation))
            }
            BeginOperation::Running => {
                apply_running(res, operation_id)?;
                return Ok(());
            }
            BeginOperation::Replay(outcome) => {
                apply_operation_outcome(res, operation_id, outcome, true)?;
                return Ok(());
            }
            BeginOperation::Conflict => {
                apply_conflict(res, operation_id)?;
                return Ok(());
            }
            BeginOperation::Full => {
                apply_registry_full(res, operation_id)?;
                return Ok(());
            }
            BeginOperation::Unavailable => {
                apply_registry_unavailable(res, operation_id)?;
                return Ok(());
            }
        };

        match endpoint {
            MKDIR_API_PATH => match serde_json::from_slice::<MkdirRequest>(&body) {
                Ok(request) => {
                    self.handle_api_mkdir(request, operation, mutation.clone(), res)
                        .await?
                }
                Err(_) => {
                    finish_api_error(
                        operation,
                        res,
                        StatusCode::BAD_REQUEST,
                        TrackedOperationError::InvalidJson,
                    )
                    .await?
                }
            },
            MOVE_API_PATH => match serde_json::from_slice::<MoveRequest>(&body) {
                Ok(request) => {
                    self.handle_api_move(owner, request, operation, mutation.clone(), res)
                        .await?
                }
                Err(_) => {
                    finish_api_error(
                        operation,
                        res,
                        StatusCode::BAD_REQUEST,
                        TrackedOperationError::InvalidJson,
                    )
                    .await?
                }
            },
            RENAME_API_PATH => match serde_json::from_slice::<RenameRequest>(&body) {
                Ok(request) => {
                    self.handle_api_rename(owner, request, operation, mutation.clone(), res)
                        .await?
                }
                Err(_) => {
                    finish_api_error(
                        operation,
                        res,
                        StatusCode::BAD_REQUEST,
                        TrackedOperationError::InvalidJson,
                    )
                    .await?
                }
            },
            _ => unreachable!(),
        }
        Ok(())
    }

    async fn upload_preflight_has_untraversable_ancestor(
        &self,
        path: &Path,
    ) -> std::io::Result<bool> {
        let mut ancestor = path.parent();
        while let Some(current) = ancestor {
            match self.content.rooted_fs.metadata(current).await {
                Ok(metadata) => return Ok(!metadata.is_dir()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) || error.raw_os_error()
                        == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
                {
                    match self.content.rooted_fs.metadata_nofollow(current).await {
                        Ok(_) => return Ok(true),
                        Err(fallback)
                            if matches!(
                                fallback.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                            ) || fallback.raw_os_error()
                                == Some(rustix::io::Errno::LOOP.raw_os_error()) => {}
                        Err(fallback)
                            if fallback.raw_os_error()
                                == Some(rustix::io::Errno::XDEV.raw_os_error()) =>
                        {
                            return Ok(true);
                        }
                        Err(fallback) => return Err(fallback),
                    }
                }
                Err(error)
                    if error.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) =>
                {
                    return Ok(true);
                }
                Err(error) => return Err(error),
            }
            if current == self.content.args.serve_path {
                return Ok(true);
            }
            ancestor = current.parent();
        }
        Ok(true)
    }

    async fn handle_upload_preflight(
        &self,
        owner: &str,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        let Some(body) = collect_untracked_api_body(req, UPLOAD_PREFLIGHT_BODY_LIMIT, res).await?
        else {
            return Ok(());
        };
        let request = match serde_json::from_slice::<UploadPreflightRequest>(&body) {
            Ok(request) => request,
            Err(_) => {
                status_api_error(
                    res,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::INVALID_JSON,
                    "Invalid upload preflight JSON",
                    None,
                )?;
                return Ok(());
            }
        };
        self.handle_upload_preflight_paths(owner, request.paths, res)
            .await
    }

    async fn handle_upload_preflight_paths(
        &self,
        owner: &str,
        paths: Vec<String>,
        res: &mut Response,
    ) -> Result<()> {
        if paths.is_empty() || paths.len() > UPLOAD_PREFLIGHT_PATH_LIMIT {
            status_api_error(
                res,
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::REQUEST_BODY_TOO_LARGE,
                "Upload preflight must contain between 1 and 512 paths",
                None,
            )?;
            return Ok(());
        }
        let mut total_path_bytes = 0_usize;
        let mut unique_paths = HashSet::with_capacity(paths.len());
        for path in &paths {
            total_path_bytes = match total_path_bytes.checked_add(path.len()) {
                Some(total) if total <= UPLOAD_PREFLIGHT_PATH_BYTES_LIMIT => total,
                _ => {
                    status_api_error(
                        res,
                        StatusCode::PAYLOAD_TOO_LARGE,
                        ErrorCode::REQUEST_BODY_TOO_LARGE,
                        "Upload preflight paths exceed the 256 KiB limit",
                        None,
                    )?;
                    return Ok(());
                }
            };
            if !unique_paths.insert(path.as_str()) {
                status_api_error(
                    res,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::INVALID_REQUEST,
                    "Upload preflight paths must be unique",
                    None,
                )?;
                return Ok(());
            }
        }
        drop(unique_paths);

        let _preflight_permit = match self
            .admission
            .upload_preflight_slots
            .clone()
            .try_acquire_owned()
        {
            Ok(permit) => permit,
            Err(_) => {
                render_problem(
                    res,
                    &ApiError::new(
                        StatusCode::TOO_MANY_REQUESTS,
                        ErrorCode::UPLOAD_PREFLIGHT_CONCURRENCY_LIMIT,
                        "Too many upload preflight requests are running",
                    )
                    .with_recovery(RecoveryAdvice::RetryAfterSeconds(1)),
                )?;
                return Ok(());
            }
        };

        let mut targets = Vec::with_capacity(paths.len());
        for logical_path in paths {
            let Some(path) = self.resolve_browser_path(&logical_path) else {
                status_api_error(
                    res,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::INVALID_PATH,
                    "Upload preflight contains an invalid path",
                    None,
                )?;
                return Ok(());
            };
            // This hook marks the boundary immediately before the first rooted
            // filesystem probe for an admitted preflight batch.
            #[cfg(test)]
            if let Some(hook) = self
                .admission
                .upload_preflight_probe_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                hook(targets.len());
            }
            let target_exists = match self.route_metadata(&path).await {
                Ok(metadata) => metadata.is_some(),
                Err(error) if is_invalid_preflight_path_error(&error) => {
                    status_api_error(
                        res,
                        StatusCode::BAD_REQUEST,
                        ErrorCode::INVALID_PATH,
                        "Upload preflight contains an invalid path",
                        None,
                    )?;
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            let has_untraversable_ancestor = match self
                .upload_preflight_has_untraversable_ancestor(&path)
                .await
            {
                Ok(invalid) => invalid,
                Err(error) if is_invalid_preflight_path_error(&error) => true,
                Err(error) => return Err(error.into()),
            };
            let escapes_root = if target_exists {
                false
            } else {
                match self.guard_root_contained(&path).await {
                    Ok(escapes) => escapes,
                    Err(error) if is_invalid_preflight_path_error(&error) => true,
                    Err(error) => return Err(error.into()),
                }
            };
            if has_untraversable_ancestor || escapes_root {
                status_api_error(
                    res,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::INVALID_PATH,
                    "Upload preflight contains an invalid path",
                    None,
                )?;
                return Ok(());
            }
            let inspection = self.inspect_upload_target(owner, &path).await?;
            targets.push(UploadPreflightTarget {
                path: logical_path,
                exists: inspection.exists,
                revision: inspection.revision.map(|revision| revision.encode()),
                replaceable: inspection.replaceable,
            });
        }
        write_json_response(res, &UploadPreflightResponse { targets })
    }

    async fn handle_upload_discard(
        &self,
        owner: &str,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        let Some(body) = collect_untracked_api_body(req, API_BODY_LIMIT, res).await? else {
            return Ok(());
        };
        let request = match serde_json::from_slice::<UploadDiscardRequest>(&body) {
            Ok(request) => request,
            Err(_) => {
                status_api_error(
                    res,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::INVALID_JSON,
                    "Invalid staged-upload discard JSON",
                    None,
                )?;
                return Ok(());
            }
        };
        let Some(upload_id) = parse_canonical_operation_id(&request.upload_id) else {
            status_api_error(
                res,
                StatusCode::BAD_REQUEST,
                ErrorCode::INVALID_UPLOAD_ID,
                "Upload ID must be a canonical UUID",
                None,
            )?;
            return Ok(());
        };
        let Some(path) = self.resolve_browser_path(&request.path) else {
            status_api_error(
                res,
                StatusCode::BAD_REQUEST,
                ErrorCode::INVALID_PATH,
                "Staged-upload discard path is invalid",
                None,
            )?;
            return Ok(());
        };
        if self.guard_root_contained(&path).await? {
            status_api_error(
                res,
                StatusCode::BAD_REQUEST,
                ErrorCode::INVALID_PATH,
                "Staged-upload discard path is invalid",
                None,
            )?;
            return Ok(());
        }
        self.discard_awaiting_upload(owner, &path, upload_id, res)
            .await
    }

    async fn handle_api_mkdir(
        &self,
        request: MkdirRequest,
        mut operation: TrackedOperation,
        mutation: MutationProgress,
        res: &mut Response,
    ) -> Result<()> {
        let path = match self.resolve_browser_path(&request.path) {
            Some(path) => path,
            None => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::BAD_REQUEST,
                    TrackedOperationError::InvalidPath,
                )
                .await?;
                return Ok(());
            }
        };
        let path_lease = self.content.path_coordinator.acquire([&path]).await;

        if self.guard_root_contained(&path).await? {
            finish_api_error(
                operation.take(),
                res,
                StatusCode::BAD_REQUEST,
                TrackedOperationError::InvalidPath,
            )
            .await?;
            return Ok(());
        }
        match self.content.rooted_fs.metadata_nofollow(&path).await {
            Ok(_) => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::PathExists,
                )
                .await?;
                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        match self.has_persisted_path_conflict(&[&path]).await {
            Ok(false) => {}
            Ok(true) => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::MkdirStateConflict,
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!(
                    "Failed to inspect durable state before creating a directory error={error:#}"
                );
                let operation_id = operation.as_ref().map(|(id, _)| *id);
                drop(operation.take());
                render_mkdir_state_unavailable(operation_id, res)?;
                return Ok(());
            }
        }

        let rooted_fs = self.content.rooted_fs.clone();
        let commit_path = path.clone();
        let (operation_id, mut operation_guard) = split_operation(operation.take());
        let create_result = self
            .run_operation_commit(mutation, async move {
                let _path_lease = path_lease;
                if let Some(operation) = operation_guard.as_mut() {
                    operation.mark_commit_started().await?;
                }
                match rooted_fs.create_directory(&commit_path).await {
                    Ok(created_ancestors) => {
                        if let Some(operation) = operation_guard {
                            operation
                                .complete(OperationOutcome::success(StatusCode::CREATED))
                                .await?;
                        }
                        Ok(Ok(created_ancestors))
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        if let Some(operation) = operation_guard {
                            operation
                                .complete(OperationOutcome::failure(
                                    StatusCode::CONFLICT,
                                    TrackedOperationError::PathExists,
                                ))
                                .await?;
                        }
                        Ok(Err(err))
                    }
                    Err(err) => Err(err.into()),
                }
            })
            .await?;
        match create_result {
            Ok(_) => {}
            Err(_) => {
                render_completed_error(
                    operation_id,
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::PathExists,
                )?;
                return Ok(());
            }
        }
        render_completed_success(operation_id, res, StatusCode::CREATED)?;
        Ok(())
    }

    async fn handle_api_move(
        &self,
        owner: &str,
        request: MoveRequest,
        mut operation: TrackedOperation,
        mutation: MutationProgress,
        res: &mut Response,
    ) -> Result<()> {
        let source = match self.resolve_browser_path(&request.source) {
            Some(path) => path,
            None => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::BAD_REQUEST,
                    TrackedOperationError::InvalidSourcePath,
                )
                .await?;
                return Ok(());
            }
        };
        let directory = match self.resolve_browser_directory(&request.directory) {
            Some(path) => path,
            None => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::BAD_REQUEST,
                    TrackedOperationError::InvalidDestinationPath,
                )
                .await?;
                return Ok(());
            }
        };
        let source_name = request
            .source
            .rsplit('/')
            .next()
            .expect("a parsed browser source has a final component");
        let destination =
            match self.resolve_browser_path(&logical_child(&request.directory, source_name)) {
                Some(path) => path,
                None => {
                    finish_api_error(
                        operation.take(),
                        res,
                        StatusCode::BAD_REQUEST,
                        TrackedOperationError::InvalidDestinationPath,
                    )
                    .await?;
                    return Ok(());
                }
            };
        self.handle_api_relocation(
            RelocationRequest {
                source,
                destination,
                destination_directory: directory,
                source_revision: request.source_revision,
                destination_revision: request.destination_revision,
                overwrite: request.overwrite,
                kind: RelocationKind::Move,
            },
            owner,
            operation,
            mutation,
            res,
        )
        .await
    }

    async fn handle_api_rename(
        &self,
        owner: &str,
        request: RenameRequest,
        mut operation: TrackedOperation,
        mutation: MutationProgress,
        res: &mut Response,
    ) -> Result<()> {
        let source = match self.resolve_browser_path(&request.source) {
            Some(path) => path,
            None => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::BAD_REQUEST,
                    TrackedOperationError::InvalidSourcePath,
                )
                .await?;
                return Ok(());
            }
        };
        if !is_single_browser_name(&request.name) {
            finish_api_error(
                operation.take(),
                res,
                StatusCode::BAD_REQUEST,
                TrackedOperationError::InvalidRenameName,
            )
            .await?;
            return Ok(());
        }
        let parent_logical = request
            .source
            .rsplit_once('/')
            .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
            .expect("a parsed browser source is slash-prefixed");
        let destination =
            match self.resolve_browser_path(&logical_child(parent_logical, &request.name)) {
                Some(path) => path,
                None => {
                    finish_api_error(
                        operation.take(),
                        res,
                        StatusCode::BAD_REQUEST,
                        TrackedOperationError::InvalidRenameName,
                    )
                    .await?;
                    return Ok(());
                }
            };
        let destination_directory = source
            .parent()
            .expect("a parsed browser source is beneath the managed root")
            .to_path_buf();
        self.handle_api_relocation(
            RelocationRequest {
                source,
                destination,
                destination_directory,
                source_revision: request.source_revision,
                destination_revision: request.destination_revision,
                overwrite: request.overwrite,
                kind: RelocationKind::Rename,
            },
            owner,
            operation,
            mutation,
            res,
        )
        .await
    }

    async fn handle_api_relocation(
        &self,
        request: RelocationRequest,
        owner: &str,
        mut operation: TrackedOperation,
        mutation: MutationProgress,
        res: &mut Response,
    ) -> Result<()> {
        let RelocationRequest {
            source,
            destination,
            destination_directory,
            source_revision,
            destination_revision,
            overwrite,
            kind,
        } = request;
        let source_revision = match source_revision.as_deref() {
            Some(value) => match TargetRevision::parse(value) {
                Some(revision) => revision,
                None => {
                    finish_api_error(
                        operation.take(),
                        res,
                        StatusCode::BAD_REQUEST,
                        TrackedOperationError::InvalidSourceRevision,
                    )
                    .await?;
                    return Ok(());
                }
            },
            None => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::PRECONDITION_REQUIRED,
                    TrackedOperationError::SourceRevisionRequired,
                )
                .await?;
                return Ok(());
            }
        };
        let destination_revision = match (overwrite, destination_revision.as_deref()) {
            (false, None) => None,
            (false, Some(_)) => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::BAD_REQUEST,
                    TrackedOperationError::InvalidDestinationRevision,
                )
                .await?;
                return Ok(());
            }
            (true, None) => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::PRECONDITION_REQUIRED,
                    TrackedOperationError::DestinationRevisionRequired,
                )
                .await?;
                return Ok(());
            }
            (true, Some(value)) => match TargetRevision::parse(value) {
                Some(revision) => Some(revision),
                None => {
                    finish_api_error(
                        operation.take(),
                        res,
                        StatusCode::BAD_REQUEST,
                        TrackedOperationError::InvalidDestinationRevision,
                    )
                    .await?;
                    return Ok(());
                }
            },
        };
        if source == destination {
            finish_api_error(
                operation.take(),
                res,
                StatusCode::BAD_REQUEST,
                TrackedOperationError::SourceEqualsDestination,
            )
            .await?;
            return Ok(());
        }
        let path_lease = self
            .content
            .path_coordinator
            .acquire([&source, &destination])
            .await;

        let revision_owner = OwnerId::persistent(owner);
        let source_identity = self.content.rooted_fs.replacement_identity(&source).await?;
        let source_relative = self.content.rooted_fs.state_relative_path(&source)?;
        let current_source_revision =
            target_revision(revision_owner, &source_relative, source_identity);
        if current_source_revision != Some(source_revision) {
            apply_revision_header(res, SOURCE_REVISION_HEADER, current_source_revision)?;
            finish_api_error(
                operation.take(),
                res,
                StatusCode::PRECONDITION_FAILED,
                TrackedOperationError::SourceChanged,
            )
            .await?;
            return Ok(());
        }

        if self.guard_root_contained(&source).await?
            || self.guard_root_contained(&destination).await?
        {
            finish_api_error(
                operation.take(),
                res,
                StatusCode::BAD_REQUEST,
                kind.invalid_path_error(),
            )
            .await?;
            return Ok(());
        }
        let source_meta = match self.content.rooted_fs.metadata_nofollow(&source).await {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::NOT_FOUND,
                    TrackedOperationError::SourceNotFound,
                )
                .await?;
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        if source_meta.is_dir() && destination.starts_with(&source) {
            finish_api_error(
                operation.take(),
                res,
                StatusCode::CONFLICT,
                TrackedOperationError::DirectoryIntoItself,
            )
            .await?;
            return Ok(());
        }
        match self
            .content
            .rooted_fs
            .metadata(&destination_directory)
            .await
        {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::DestinationNotDirectory,
                )
                .await?;
                return Ok(());
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::NOT_FOUND,
                    TrackedOperationError::DestinationDirectoryNotFound,
                )
                .await?;
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        }
        let destination_identity = self
            .content
            .rooted_fs
            .replacement_identity(&destination)
            .await?;
        let destination_relative = self.content.rooted_fs.state_relative_path(&destination)?;
        let current_destination_revision =
            target_revision(revision_owner, &destination_relative, destination_identity);
        if overwrite && current_destination_revision != destination_revision {
            apply_revision_header(res, TARGET_REVISION_HEADER, current_destination_revision)?;
            finish_api_error(
                operation.take(),
                res,
                StatusCode::PRECONDITION_FAILED,
                TrackedOperationError::DestinationChanged,
            )
            .await?;
            return Ok(());
        }
        let destination_meta = if destination_identity.exists() {
            Some(
                self.content
                    .rooted_fs
                    .metadata_nofollow(&destination)
                    .await?,
            )
        } else {
            None
        };
        if let Some(destination_meta) = &destination_meta {
            if source_meta.is_dir() || destination_meta.is_dir() {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::DirectoryOverwriteForbidden,
                )
                .await?;
                return Ok(());
            }
            if !overwrite {
                apply_revision_header(res, TARGET_REVISION_HEADER, current_destination_revision)?;
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::DestinationExists,
                )
                .await?;
                return Ok(());
            }
            if source_meta.dev() == destination_meta.dev()
                && source_meta.ino() == destination_meta.ino()
            {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::SourceEqualsDestination,
                )
                .await?;
                return Ok(());
            }
        }

        if self.guard_root_contained(&destination).await? {
            finish_api_error(
                operation.take(),
                res,
                StatusCode::BAD_REQUEST,
                kind.invalid_path_error(),
            )
            .await?;
            return Ok(());
        }

        match self
            .has_persisted_path_conflict(&[&source, &destination])
            .await
        {
            Ok(false) => {}
            Ok(true) => {
                finish_api_error(
                    operation.take(),
                    res,
                    StatusCode::CONFLICT,
                    kind.state_conflict_error(),
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!(
                    "Failed to inspect durable state before {} error={error:#}",
                    kind.verb()
                );
                let operation_id = operation.as_ref().map(|(id, _)| *id);
                // The commit marker and filesystem mutation have not started.
                // Dropping a Reserved operation abandons it, making a retry
                // safe once the state store is available again.
                drop(operation.take());
                render_relocation_state_unavailable(kind, operation_id, res)?;
                return Ok(());
            }
        }

        let rooted_fs = self.content.rooted_fs.clone();
        let commit_source = source.clone();
        let commit_destination = destination.clone();
        let (operation_id, mut operation_guard) = split_operation(operation.take());
        let relocation_outcome = self
            .run_operation_commit(mutation, async move {
                let _path_lease = path_lease;
                if let Some(operation) = operation_guard.as_mut() {
                    operation.mark_commit_started().await?;
                }
                let relocation_outcome = commit_relocation(
                    &rooted_fs,
                    &commit_source,
                    &commit_destination,
                    source_identity,
                    overwrite.then_some(destination_identity),
                )
                .await?;
                if let Some(operation) = operation_guard {
                    let outcome = match relocation_outcome {
                        RelocationCommitOutcome::Relocated => {
                            OperationOutcome::success(StatusCode::NO_CONTENT)
                        }
                        RelocationCommitOutcome::DestinationExists => OperationOutcome::failure(
                            StatusCode::CONFLICT,
                            TrackedOperationError::DestinationExists,
                        ),
                        RelocationCommitOutcome::SameFile => OperationOutcome::failure(
                            StatusCode::CONFLICT,
                            TrackedOperationError::SourceEqualsDestination,
                        ),
                        RelocationCommitOutcome::SourceChanged => OperationOutcome::failure(
                            StatusCode::PRECONDITION_FAILED,
                            TrackedOperationError::SourceChanged,
                        ),
                        RelocationCommitOutcome::DestinationChanged => OperationOutcome::failure(
                            StatusCode::PRECONDITION_FAILED,
                            TrackedOperationError::DestinationChanged,
                        ),
                    };
                    operation.complete(outcome).await?;
                }
                Ok(relocation_outcome)
            })
            .await?;
        match relocation_outcome {
            RelocationCommitOutcome::Relocated => {}
            RelocationCommitOutcome::DestinationExists => {
                render_completed_error(
                    operation_id,
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::DestinationExists,
                )?;
                return Ok(());
            }
            RelocationCommitOutcome::SameFile => {
                render_completed_error(
                    operation_id,
                    res,
                    StatusCode::CONFLICT,
                    TrackedOperationError::SourceEqualsDestination,
                )?;
                return Ok(());
            }
            RelocationCommitOutcome::SourceChanged => {
                match self.content.rooted_fs.replacement_identity(&source).await {
                    Ok(identity) => apply_revision_header(
                        res,
                        SOURCE_REVISION_HEADER,
                        target_revision(revision_owner, &source_relative, identity),
                    )?,
                    Err(error) => log::warn!(
                        "Failed to inspect the current relocation source revision path={} error={error:#}",
                        source.display()
                    ),
                }
                render_completed_error(
                    operation_id,
                    res,
                    StatusCode::PRECONDITION_FAILED,
                    TrackedOperationError::SourceChanged,
                )?;
                return Ok(());
            }
            RelocationCommitOutcome::DestinationChanged => {
                match self
                    .content
                    .rooted_fs
                    .replacement_identity(&destination)
                    .await
                {
                    Ok(identity) => apply_revision_header(
                        res,
                        TARGET_REVISION_HEADER,
                        target_revision(revision_owner, &destination_relative, identity),
                    )?,
                    Err(error) => log::warn!(
                        "Failed to inspect the current relocation destination revision path={} error={error:#}",
                        destination.display()
                    ),
                }
                render_completed_error(
                    operation_id,
                    res,
                    StatusCode::PRECONDITION_FAILED,
                    TrackedOperationError::DestinationChanged,
                )?;
                return Ok(());
            }
        }

        render_completed_success(operation_id, res, StatusCode::NO_CONTENT)?;
        Ok(())
    }

    fn resolve_browser_path(&self, path: &str) -> Option<PathBuf> {
        self.content
            .path_policy
            .parse_browser_target(path)
            .map(|path| path.into_path_buf())
    }

    fn resolve_browser_directory(&self, path: &str) -> Option<PathBuf> {
        self.content
            .path_policy
            .parse_list_target(path)
            .map(|path| path.into_path_buf())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelocationKind {
    Move,
    Rename,
}

impl RelocationKind {
    const fn verb(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Rename => "rename",
        }
    }

    const fn invalid_path_error(self) -> TrackedOperationError {
        match self {
            Self::Move => TrackedOperationError::InvalidMovePath,
            Self::Rename => TrackedOperationError::InvalidRenamePath,
        }
    }

    const fn state_conflict_error(self) -> TrackedOperationError {
        match self {
            Self::Move => TrackedOperationError::MoveStateConflict,
            Self::Rename => TrackedOperationError::RenameStateConflict,
        }
    }

    const fn state_unavailable_code(self) -> ErrorCode {
        match self {
            Self::Move => ErrorCode::MOVE_STATE_UNAVAILABLE,
            Self::Rename => ErrorCode::RENAME_STATE_UNAVAILABLE,
        }
    }

    const fn state_unavailable_detail(self) -> &'static str {
        match self {
            Self::Move => "Move safety state is temporarily unavailable",
            Self::Rename => "Rename safety state is temporarily unavailable",
        }
    }
}

struct RelocationRequest {
    source: PathBuf,
    destination: PathBuf,
    destination_directory: PathBuf,
    source_revision: Option<String>,
    destination_revision: Option<String>,
    overwrite: bool,
    kind: RelocationKind,
}

pub(in crate::server) fn apply_revision_header(
    res: &mut Response,
    name: &'static str,
    revision: Option<TargetRevision>,
) -> Result<()> {
    if let Some(revision) = revision {
        res.headers_mut()
            .insert(name, HeaderValue::from_str(&revision.encode())?);
    } else {
        res.headers_mut().remove(name);
    }
    Ok(())
}

fn is_single_browser_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= BROWSER_COMPONENT_BYTES_LIMIT
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0')
}

fn is_invalid_preflight_path_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == rustix::io::Errno::LOOP.raw_os_error()
                || code == rustix::io::Errno::NAMETOOLONG.raw_os_error()
    )
}

fn logical_child(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{directory}/{name}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelocationCommitOutcome {
    Relocated,
    SourceChanged,
    DestinationChanged,
    DestinationExists,
    SameFile,
}

async fn commit_relocation(
    rooted_fs: &RootedFs,
    source: &Path,
    destination: &Path,
    expected_source: ReplacementTargetIdentity,
    expected_destination: Option<ReplacementTargetIdentity>,
) -> Result<RelocationCommitOutcome> {
    Ok(
        match rooted_fs
            .rename_if_unchanged(source, destination, expected_source, expected_destination)
            .await?
        {
            CheckedRelocationOutcome::Relocated => RelocationCommitOutcome::Relocated,
            CheckedRelocationOutcome::SourceChanged => RelocationCommitOutcome::SourceChanged,
            CheckedRelocationOutcome::DestinationChanged => {
                RelocationCommitOutcome::DestinationChanged
            }
            CheckedRelocationOutcome::DestinationExists => {
                RelocationCommitOutcome::DestinationExists
            }
            CheckedRelocationOutcome::SameFile => RelocationCommitOutcome::SameFile,
        },
    )
}

#[cfg(test)]
mod tests;
