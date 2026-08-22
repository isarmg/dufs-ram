use super::{MutationProgress, render_upload_problem};
use crate::{
    auth::{SessionInfo, session_token_from_cookie},
    http_utils::body_full,
    request_context::RequestContext,
    server::{
        HEALTH_CHECK_PATH, READINESS_CHECK_PATH, Request, Response, Server,
        browser_api::{
            BROWSER_API_PREFIX, SOURCE_REVISION_HEADER, apply_revision_header,
            is_browser_api_endpoint,
        },
        delete::DeleteRequest,
        head_only_for,
        identity::OwnerId,
        listing::LIST_API_PATH,
        operation_registry::{
            BeginOperation, JOB_STATUS_PREFIX, OperationFingerprint, OperationGuard,
            OperationOutcome, TrackedOperationError, apply_conflict, apply_invalid_id,
            apply_invalid_job_id, apply_operation_outcome, apply_registry_full,
            apply_registry_unavailable, apply_running, apply_status, parse_canonical_operation_id,
            parse_operation_id, set_operation_headers,
        },
        path_coordinator::PathLease,
        path_policy::{RootedPath, RoutePath},
        problem::{ApiError, ErrorCode, OperationProblemContext, RecoveryAdvice, render_problem},
        protocol::{OperationPublicState, UploadPublicState},
        session::{LOGIN_ERROR_QUERY, LOGIN_PATH, LOGOUT_PATH},
        status_bad_request, status_csrf_forbid, status_method_not_allowed, status_not_found,
        upload::{
            TargetRevision, UploadMode, UploadOptions, UploadOverwritePolicy, parse_upload_id,
            parse_upload_length, parse_upload_offset, parse_upload_overwrite, target_revision,
        },
    },
};

use anyhow::Result;
use hyper::{
    HeaderMap, Method, StatusCode, Uri,
    header::{COOKIE, IF_MATCH},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, timeout_at};

impl Server {
    pub(super) async fn handle_inner(
        self: Arc<Self>,
        req: Request,
        relative_path: Option<RoutePath>,
        internal_api_request: bool,
        mutation: MutationProgress,
        context: &mut RequestContext,
    ) -> Result<Response> {
        let route_path = match relative_path {
            Some(value) => value,
            None => {
                let mut res = Response::default();
                status_bad_request(&mut res, "Invalid Path");
                return Ok(res);
            }
        };
        RequestDispatcher::new(
            self,
            req,
            route_path,
            internal_api_request,
            mutation,
            context,
        )
        .dispatch()
        .await
    }
}

#[derive(Clone, Copy)]
struct UploadRequest {
    id: uuid::Uuid,
    length: u64,
    mode: UploadMode,
    overwrite: UploadOverwritePolicy,
}

#[derive(Clone, Copy, Default)]
struct MutationHeaders {
    delete_operation_id: Option<uuid::Uuid>,
    delete_condition: DeleteCondition,
    upload: Option<UploadRequest>,
}

#[derive(Clone, Copy, Default)]
enum DeleteCondition {
    #[default]
    Missing,
    Invalid,
    Revision(TargetRevision),
}

struct AuthenticatedRequest {
    token: String,
    session: SessionInfo,
}

enum DeletePreparation {
    NotRequested,
    Started(uuid::Uuid, OperationGuard),
    Complete,
}

struct PreparedTarget {
    path: RootedPath,
    is_miss: bool,
    miss_is_hidden: bool,
    is_dir: bool,
    is_file: bool,
    delete_operation: Option<(uuid::Uuid, OperationGuard)>,
    path_lease: Option<PathLease>,
    upload_permit: Option<OwnedSemaphorePermit>,
    upload_deadline: Option<Instant>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Continue,
    Complete,
}

/// Owns the HTTP-boundary facts for exactly one request. Route phases consume
/// this shared classification instead of independently rediscovering whether
/// a request is an internal API, upload, or tracked mutation.
struct RequestDispatcher<'a> {
    server: Arc<Server>,
    request: Option<Request>,
    route_path: RoutePath,
    internal_api_request: bool,
    mutation: MutationProgress,
    context: &'a mut RequestContext,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    request_path: String,
    query_params: HashMap<String, String>,
    response: Response,
}

impl<'a> RequestDispatcher<'a> {
    fn new(
        server: Arc<Server>,
        request: Request,
        route_path: RoutePath,
        internal_api_request: bool,
        mutation: MutationProgress,
        context: &'a mut RequestContext,
    ) -> Self {
        let method = request.method().clone();
        let uri = request.uri().clone();
        let headers = request.headers().clone();
        let request_path = uri.path().to_owned();
        let query_params = form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        Self {
            server,
            request: Some(request),
            route_path,
            internal_api_request,
            mutation,
            context,
            method,
            uri,
            headers,
            request_path,
            query_params,
            response: Response::default(),
        }
    }

    async fn dispatch(mut self) -> Result<Response> {
        if self.dispatch_public_routes().await? == Phase::Complete {
            return Ok(self.response);
        }
        let Some(authenticated) = self.authenticate()? else {
            return Ok(self.response);
        };
        if self.enforce_csrf(&authenticated)? == Phase::Complete
            || self.dispatch_authenticated_api(&authenticated).await? == Phase::Complete
        {
            return Ok(self.response);
        }
        self.dispatch_content(&authenticated.session).await?;
        Ok(self.response)
    }

    async fn dispatch_public_routes(&mut self) -> Result<Phase> {
        // Liveness deliberately sits outside authentication and exposes no
        // filesystem or account detail. Readiness remains authenticated.
        if self.route_path.as_str() == HEALTH_CHECK_PATH {
            if matches!(self.method, Method::GET | Method::HEAD) {
                self.server
                    .send_liveness(head_only_for(&self.method), &mut self.response);
            } else {
                status_method_not_allowed(&mut self.response, "GET, HEAD");
            }
            return Ok(Phase::Complete);
        }

        if self.route_path.as_str() == LOGIN_PATH {
            match self.method {
                Method::GET => self.server.send_login_page_for_get(
                    self.query_params.get(LOGIN_ERROR_QUERY).map(String::as_str),
                    &mut self.response,
                )?,
                Method::POST => {
                    let peer_ip = self.context.peer().ip();
                    let request = self.take_request();
                    if let Some(user) = self
                        .server
                        .handle_login(request, peer_ip, &mut self.response)
                        .await?
                    {
                        self.server
                            .content
                            .args
                            .http_logger
                            .set_authenticated_user(self.context.access_log_mut(), &user);
                    }
                }
                _ => status_method_not_allowed(&mut self.response, "GET, POST"),
            }
            return Ok(Phase::Complete);
        }

        // Content-addressed assets must remain available to the login page.
        if matches!(self.method, Method::GET | Method::HEAD)
            && self.server.handle_internal(
                self.route_path.as_str(),
                head_only_for(&self.method),
                &mut self.response,
            )
        {
            return Ok(Phase::Complete);
        }
        Ok(Phase::Continue)
    }

    fn authenticate(&mut self) -> Result<Option<AuthenticatedRequest>> {
        let token = self
            .headers
            .get(COOKIE)
            .and_then(session_token_from_cookie)
            .map(str::to_owned);
        let authenticated = token.and_then(|token| {
            self.server
                .content
                .auth
                .authenticate(&token)
                .map(|session| AuthenticatedRequest { token, session })
        });
        let Some(authenticated) = authenticated else {
            self.server.reject_unauthenticated(
                &self.method,
                &self.headers,
                self.internal_api_request,
                &mut self.response,
            )?;
            return Ok(None);
        };
        self.server
            .content
            .args
            .http_logger
            .set_authenticated_user(self.context.access_log_mut(), &authenticated.session.user);
        Ok(Some(authenticated))
    }

    fn enforce_csrf(&mut self, authenticated: &AuthenticatedRequest) -> Result<Phase> {
        if !matches!(
            self.method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        ) || self.server.csrf_is_valid(
            &self.headers,
            &self.uri,
            self.context.peer().ip(),
            &authenticated.token,
            &authenticated.session.csrf_token,
        ) {
            return Ok(Phase::Continue);
        }

        status_csrf_forbid(&mut self.response);
        if self.internal_api_request {
            render_problem(
                &mut self.response,
                &ApiError::new(
                    StatusCode::FORBIDDEN,
                    ErrorCode::CSRF_FAILED,
                    "CSRF validation failed",
                ),
            )?;
        }
        Ok(Phase::Complete)
    }

    async fn dispatch_authenticated_api(
        &mut self,
        authenticated: &AuthenticatedRequest,
    ) -> Result<Phase> {
        let relative_path = self.route_path.as_str().to_owned();
        if self.method == Method::POST && relative_path == LOGOUT_PATH {
            self.server
                .handle_logout(&authenticated.token, &mut self.response);
            return Ok(Phase::Complete);
        }
        if relative_path == READINESS_CHECK_PATH {
            if matches!(self.method, Method::GET | Method::HEAD) {
                self.server
                    .send_readiness(head_only_for(&self.method), &mut self.response)
                    .await;
            } else {
                status_method_not_allowed(&mut self.response, "GET, HEAD");
            }
            return Ok(Phase::Complete);
        }
        if let Some(operation_id) = relative_path.strip_prefix(JOB_STATUS_PREFIX) {
            self.dispatch_job_status(operation_id, &authenticated.session.user)
                .await?;
            return Ok(Phase::Complete);
        }
        if relative_path == LIST_API_PATH {
            if self.method == Method::GET {
                self.server
                    .handle_list_api(
                        &authenticated.session.user,
                        &self.query_params,
                        &mut self.response,
                    )
                    .await?;
            } else {
                render_api_method_not_allowed(&mut self.response, "GET")?;
            }
            return Ok(Phase::Complete);
        }
        if relative_path.starts_with(BROWSER_API_PREFIX) {
            self.dispatch_browser_api(&authenticated.session.user)
                .await?;
            return Ok(Phase::Complete);
        }
        Ok(Phase::Continue)
    }

    async fn dispatch_job_status(&mut self, id: &str, owner: &str) -> Result<()> {
        if self.method != Method::GET {
            return render_api_method_not_allowed(&mut self.response, "GET");
        }
        let Some(id) = parse_canonical_operation_id(id) else {
            return apply_invalid_job_id(
                &mut self.response,
                "Job status path must end in a canonical UUID",
            );
        };
        let status = self
            .server
            .state
            .operation_registry
            .status(owner, id)
            .await?;
        apply_status(&mut self.response, id, status)
    }

    async fn dispatch_browser_api(&mut self, owner: &str) -> Result<()> {
        let relative_path = self.route_path.as_str().to_owned();
        if self.method == Method::POST {
            let request = self.take_request();
            self.server
                .handle_browser_api(
                    &relative_path,
                    owner,
                    request,
                    self.mutation.clone(),
                    &mut self.response,
                )
                .await
        } else if is_browser_api_endpoint(&relative_path) {
            render_api_method_not_allowed(&mut self.response, "POST")
        } else {
            render_problem(
                &mut self.response,
                &ApiError::new(
                    StatusCode::NOT_FOUND,
                    ErrorCode::API_ENDPOINT_NOT_FOUND,
                    "API endpoint not found",
                ),
            )
        }
    }

    async fn dispatch_content(&mut self, session: &SessionInfo) -> Result<Phase> {
        let Some(headers) = self.parse_mutation_headers()? else {
            return Ok(Phase::Complete);
        };
        if self.reject_reserved_target(headers)? == Phase::Complete {
            return Ok(Phase::Complete);
        }
        let path = self.server.join_path(&self.route_path);
        if self.reject_root_delete(&path, headers)? == Phase::Complete {
            return Ok(Phase::Complete);
        }
        let Some(mut target) = self.prepare_target(&path, headers, &session.user).await? else {
            return Ok(Phase::Complete);
        };
        if self.reject_hidden_target(&mut target, headers).await? == Phase::Complete
            || self.dispatch_upload_status(&target, &session.user).await? == Phase::Complete
        {
            return Ok(Phase::Complete);
        }
        self.dispatch_target_method(&mut target, headers, session)
            .await?;
        Ok(Phase::Complete)
    }

    fn parse_mutation_headers(&mut self) -> Result<Option<MutationHeaders>> {
        let delete_operation_id = if self.method == Method::DELETE {
            match parse_operation_id(&self.headers) {
                Ok(Some(id)) => Some(id),
                Ok(None) => {
                    apply_invalid_id(
                        &mut self.response,
                        "The x-dufs-operation-id header is required",
                    )?;
                    return Ok(None);
                }
                Err(message) => {
                    apply_invalid_id(&mut self.response, message)?;
                    return Ok(None);
                }
            }
        } else {
            None
        };
        let upload = if matches!(self.method, Method::PUT | Method::PATCH) {
            let Some(id) = self.parse_required_upload_id()? else {
                return Ok(None);
            };
            let Some(length) = self.parse_required_upload_length()? else {
                return Ok(None);
            };
            let Some(mode) = self.parse_upload_mode()? else {
                return Ok(None);
            };
            let Some(overwrite) = self.parse_upload_overwrite()? else {
                return Ok(None);
            };
            Some(UploadRequest {
                id,
                length,
                mode,
                overwrite,
            })
        } else {
            None
        };
        let delete_condition = if self.method == Method::DELETE {
            parse_delete_condition(&self.headers)
        } else {
            DeleteCondition::Missing
        };
        Ok(Some(MutationHeaders {
            delete_operation_id,
            delete_condition,
            upload,
        }))
    }

    fn parse_required_upload_id(&mut self) -> Result<Option<uuid::Uuid>> {
        match parse_upload_id(&self.headers) {
            Ok(Some(value)) => Ok(Some(value)),
            Ok(None) => {
                self.render_bad_upload_header(
                    ErrorCode::INVALID_UPLOAD_ID,
                    "The x-dufs-upload-id header is required",
                )?;
                Ok(None)
            }
            Err(error) => {
                self.render_bad_upload_header(ErrorCode::INVALID_UPLOAD_ID, error.to_string())?;
                Ok(None)
            }
        }
    }

    fn parse_required_upload_length(&mut self) -> Result<Option<u64>> {
        match parse_upload_length(&self.headers) {
            Ok(Some(value)) => Ok(Some(value)),
            Ok(None) => {
                self.render_bad_upload_header(
                    ErrorCode::INVALID_UPLOAD_LENGTH,
                    "The x-dufs-upload-length header is required",
                )?;
                Ok(None)
            }
            Err(error) => {
                self.render_bad_upload_header(ErrorCode::INVALID_UPLOAD_LENGTH, error.to_string())?;
                Ok(None)
            }
        }
    }

    fn parse_upload_mode(&mut self) -> Result<Option<UploadMode>> {
        if self.method != Method::PATCH {
            return Ok(Some(UploadMode::Fresh));
        }
        match parse_upload_offset(&self.headers) {
            Ok(Some(offset)) => Ok(Some(UploadMode::Resume { offset })),
            Ok(None) => {
                self.render_bad_upload_header(
                    ErrorCode::INVALID_UPLOAD_OFFSET,
                    "The x-dufs-upload-offset header is required",
                )?;
                Ok(None)
            }
            Err(error) => {
                self.render_bad_upload_header(ErrorCode::INVALID_UPLOAD_OFFSET, error.to_string())?;
                Ok(None)
            }
        }
    }

    fn parse_upload_overwrite(&mut self) -> Result<Option<UploadOverwritePolicy>> {
        match parse_upload_overwrite(&self.headers) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                let code = if error.to_string().contains("target-revision") {
                    ErrorCode::INVALID_TARGET_REVISION
                } else {
                    ErrorCode::INVALID_UPLOAD_OVERWRITE
                };
                self.render_bad_upload_header(code, error.to_string())?;
                Ok(None)
            }
        }
    }

    fn render_bad_upload_header(
        &mut self,
        code: ErrorCode,
        detail: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Result<()> {
        render_problem(
            &mut self.response,
            &ApiError::new(StatusCode::BAD_REQUEST, code, detail),
        )
    }

    fn reject_reserved_target(&mut self, headers: MutationHeaders) -> Result<Phase> {
        let reserved = self
            .route_path
            .split('/')
            .next()
            .is_some_and(|part| self.server.is_reserved_internal_component(part));
        if !reserved {
            return Ok(Phase::Continue);
        }
        if let Some(upload) = headers.upload {
            self.render_upload_rejection(
                upload,
                StatusCode::NOT_FOUND,
                ErrorCode::UPLOAD_TARGET_NOT_FOUND,
                "Upload target not found",
                RecoveryAdvice::None,
            )?;
        } else if self.method == Method::DELETE {
            apply_tracked_delete_rejection(
                headers
                    .delete_operation_id
                    .expect("DELETE operation ID was validated"),
                &mut self.response,
                StatusCode::NOT_FOUND,
                ErrorCode::TARGET_NOT_FOUND,
                "Target not found",
            )?;
        } else {
            status_not_found(&mut self.response);
        }
        Ok(Phase::Complete)
    }

    fn reject_root_delete(&mut self, path: &RootedPath, headers: MutationHeaders) -> Result<Phase> {
        if self.method != Method::DELETE || !self.server.is_managed_root(path.as_path()) {
            return Ok(Phase::Continue);
        }
        apply_tracked_delete_rejection(
            headers
                .delete_operation_id
                .expect("DELETE operation ID was validated"),
            &mut self.response,
            StatusCode::FORBIDDEN,
            ErrorCode::ROOT_DELETE_FORBIDDEN,
            "Shared root cannot be deleted",
        )?;
        Ok(Phase::Complete)
    }

    async fn prepare_target(
        &mut self,
        path: &RootedPath,
        headers: MutationHeaders,
        owner: &str,
    ) -> Result<Option<PreparedTarget>> {
        let delete_operation = match self.begin_delete(headers, owner).await? {
            DeletePreparation::NotRequested => None,
            DeletePreparation::Started(id, operation) => Some((id, operation)),
            DeletePreparation::Complete => return Ok(None),
        };
        self.prepare_target_after_delete(path, headers, delete_operation)
            .await
    }

    async fn begin_delete(
        &mut self,
        headers: MutationHeaders,
        owner: &str,
    ) -> Result<DeletePreparation> {
        if self.method != Method::DELETE {
            return Ok(DeletePreparation::NotRequested);
        }
        let id = headers
            .delete_operation_id
            .expect("DELETE operation ID was validated");
        let revision = match headers.delete_condition {
            DeleteCondition::Revision(revision) => revision,
            DeleteCondition::Missing => {
                apply_tracked_delete_rejection(
                    id,
                    &mut self.response,
                    StatusCode::PRECONDITION_REQUIRED,
                    ErrorCode::SOURCE_REVISION_REQUIRED,
                    "The If-Match header is required",
                )?;
                return Ok(DeletePreparation::Complete);
            }
            DeleteCondition::Invalid => {
                apply_tracked_delete_rejection(
                    id,
                    &mut self.response,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::INVALID_SOURCE_REVISION,
                    "If-Match must contain one strong quoted target revision",
                )?;
                return Ok(DeletePreparation::Complete);
            }
        };
        let revision_bytes = revision.into_bytes();
        let fingerprint =
            OperationFingerprint::new(&[b"DELETE", self.route_path.as_bytes(), &revision_bytes]);
        match self
            .server
            .state
            .operation_registry
            .begin(owner, id, fingerprint)
            .await?
        {
            BeginOperation::Started(operation) => {
                self.mutation.mark_reserved();
                Ok(DeletePreparation::Started(id, operation))
            }
            BeginOperation::Running => {
                apply_running(&mut self.response, id)?;
                Ok(DeletePreparation::Complete)
            }
            BeginOperation::Replay(outcome) => {
                apply_operation_outcome(&mut self.response, id, outcome, true)?;
                Ok(DeletePreparation::Complete)
            }
            BeginOperation::Conflict => {
                apply_conflict(&mut self.response, id)?;
                Ok(DeletePreparation::Complete)
            }
            BeginOperation::Full => {
                apply_registry_full(&mut self.response, id)?;
                Ok(DeletePreparation::Complete)
            }
            BeginOperation::Unavailable => {
                apply_registry_unavailable(&mut self.response, id)?;
                Ok(DeletePreparation::Complete)
            }
        }
    }

    async fn prepare_target_after_delete(
        &mut self,
        path: &RootedPath,
        headers: MutationHeaders,
        delete_operation: Option<(uuid::Uuid, OperationGuard)>,
    ) -> Result<Option<PreparedTarget>> {
        let upload_deadline = headers.upload.map(|_| {
            Instant::now()
                .checked_add(Duration::from_secs(
                    self.server.content.args.upload_total_timeout,
                ))
                .expect("upload timeout was validated at startup")
        });
        let Some((mut path_lease, mut upload_permit)) = self
            .acquire_target_admission(path, headers.upload, upload_deadline)
            .await?
        else {
            return Ok(None);
        };
        let Some((metadata, upload_miss_is_hidden)) = self
            .inspect_target(
                path,
                headers.upload,
                upload_deadline,
                &mut path_lease,
                &mut upload_permit,
            )
            .await?
        else {
            return Ok(None);
        };
        let (is_miss, is_dir, is_file) = metadata
            .map(|metadata| (false, metadata.is_dir(), metadata.is_file()))
            .unwrap_or((true, false, false));
        let miss_is_hidden = is_miss
            && match upload_miss_is_hidden {
                Some(value) => value,
                None => self.server.guard_root_contained(path.as_path()).await?,
            };
        Ok(Some(PreparedTarget {
            path: path.clone(),
            is_miss,
            miss_is_hidden,
            is_dir,
            is_file,
            delete_operation,
            path_lease,
            upload_permit,
            upload_deadline,
        }))
    }

    async fn acquire_target_admission(
        &mut self,
        path: &RootedPath,
        upload: Option<UploadRequest>,
        deadline: Option<Instant>,
    ) -> Result<Option<(Option<PathLease>, Option<OwnedSemaphorePermit>)>> {
        // Path-conflicting uploads wait before consuming a global slot.
        let path_lease = if let Some(upload) = upload {
            match timeout_at(
                deadline.expect("upload request has a deadline"),
                self.server
                    .content
                    .path_coordinator
                    .acquire([path.as_path()]),
            )
            .await
            {
                Ok(lease) => Some(lease),
                Err(_) => {
                    self.render_upload_rejection(
                        upload,
                        StatusCode::REQUEST_TIMEOUT,
                        ErrorCode::UPLOAD_PATH_WAIT_TIMEOUT,
                        "Upload timed out while waiting for the target path",
                        RecoveryAdvice::Retry,
                    )?;
                    return Ok(None);
                }
            }
        } else if self.method == Method::DELETE {
            Some(
                self.server
                    .content
                    .path_coordinator
                    .acquire([path.as_path()])
                    .await,
            )
        } else {
            None
        };
        let upload_permit = if let Some(upload) = upload {
            match self
                .server
                .admission
                .upload_slots
                .clone()
                .try_acquire_owned()
            {
                Ok(permit) => Some(permit),
                Err(_) => {
                    self.render_upload_rejection(
                        upload,
                        StatusCode::TOO_MANY_REQUESTS,
                        ErrorCode::UPLOAD_CONCURRENCY_LIMIT,
                        "Too many concurrent uploads",
                        RecoveryAdvice::RetryAfterSeconds(1),
                    )?;
                    return Ok(None);
                }
            }
        } else {
            None
        };
        Ok(Some((path_lease, upload_permit)))
    }

    async fn inspect_target(
        &mut self,
        path: &RootedPath,
        upload: Option<UploadRequest>,
        deadline: Option<Instant>,
        path_lease: &mut Option<PathLease>,
        upload_permit: &mut Option<OwnedSemaphorePermit>,
    ) -> Result<Option<(Option<std::fs::Metadata>, Option<bool>)>> {
        let Some(upload) = upload else {
            return Ok(Some((
                self.server.route_metadata(path.as_path()).await?,
                None,
            )));
        };
        // Keep both leases in a tracked task if the blocking metadata probe
        // outlives the HTTP upload deadline.
        let server = self.server.clone();
        let metadata_path = path.as_path().to_path_buf();
        let retained_path_lease = path_lease
            .take()
            .expect("upload acquired its path lease before metadata");
        let retained_upload_permit = upload_permit
            .take()
            .expect("upload acquired its global permit before metadata");
        let mut task = self.server.lifecycle.commit_tasks.spawn(async move {
            let preparation = async {
                let metadata = server.route_metadata(&metadata_path).await?;
                let hidden = if metadata.is_none() {
                    server.guard_root_contained(&metadata_path).await?
                } else {
                    false
                };
                Ok::<_, std::io::Error>((metadata, hidden))
            }
            .await;
            (preparation, retained_path_lease, retained_upload_permit)
        });
        match timeout_at(deadline.expect("upload request has a deadline"), &mut task).await {
            Ok(result) => {
                let (preparation, retained_path_lease, retained_upload_permit) = result?;
                *path_lease = Some(retained_path_lease);
                *upload_permit = Some(retained_upload_permit);
                let (metadata, hidden) = preparation?;
                Ok(Some((metadata, Some(hidden))))
            }
            Err(_) => {
                self.render_upload_rejection(
                    upload,
                    StatusCode::REQUEST_TIMEOUT,
                    ErrorCode::UPLOAD_TARGET_INSPECTION_TIMEOUT,
                    "Upload timed out while inspecting the target",
                    RecoveryAdvice::Retry,
                )?;
                Ok(None)
            }
        }
    }

    async fn reject_hidden_target(
        &mut self,
        target: &mut PreparedTarget,
        headers: MutationHeaders,
    ) -> Result<Phase> {
        if !target.miss_is_hidden {
            return Ok(Phase::Continue);
        }
        if self.method == Method::DELETE {
            let (id, operation) = target
                .delete_operation
                .take()
                .expect("DELETE operation was started");
            let outcome = OperationOutcome::failure(
                StatusCode::NOT_FOUND,
                TrackedOperationError::TargetNotFound,
            );
            operation.complete(outcome).await?;
            apply_operation_outcome(&mut self.response, id, outcome, false)?;
        } else if let Some(upload) = headers.upload {
            self.render_upload_rejection(
                upload,
                StatusCode::NOT_FOUND,
                ErrorCode::UPLOAD_TARGET_NOT_FOUND,
                "Upload target not found",
                RecoveryAdvice::None,
            )?;
        } else {
            status_not_found(&mut self.response);
        }
        Ok(Phase::Complete)
    }

    async fn dispatch_upload_status(
        &mut self,
        target: &PreparedTarget,
        owner: &str,
    ) -> Result<Phase> {
        if self.method != Method::HEAD {
            return Ok(Phase::Continue);
        }
        match parse_upload_id(&self.headers) {
            Ok(Some(id)) => {
                let _lease = self
                    .server
                    .content
                    .path_coordinator
                    .acquire([target.path.as_path()])
                    .await;
                self.server
                    .handle_upload_status(target.path.as_path(), owner, id, &mut self.response)
                    .await?;
                Ok(Phase::Complete)
            }
            Ok(None) => Ok(Phase::Continue),
            Err(error) => {
                self.render_bad_upload_header(ErrorCode::INVALID_UPLOAD_ID, error.to_string())?;
                Ok(Phase::Complete)
            }
        }
    }

    async fn dispatch_target_method(
        &mut self,
        target: &mut PreparedTarget,
        headers: MutationHeaders,
        session: &SessionInfo,
    ) -> Result<()> {
        match self.method {
            Method::GET | Method::HEAD => self.dispatch_content_read(target, session).await,
            Method::PUT => self.dispatch_fresh_upload(target, headers, session).await,
            Method::PATCH => self.dispatch_resumed_upload(target, headers, session).await,
            Method::DELETE => self.dispatch_delete(target, headers, session).await,
            _ => {
                status_method_not_allowed(&mut self.response, "GET, HEAD, PUT, PATCH, DELETE");
                Ok(())
            }
        }
    }

    async fn dispatch_content_read(
        &mut self,
        target: &PreparedTarget,
        session: &SessionInfo,
    ) -> Result<()> {
        let head_only = self.method == Method::HEAD;
        let directory_shaped_target =
            target.is_dir || (target.is_miss && self.request_path.ends_with('/'));
        if directory_shaped_target && directory_archive_requested(&self.query_params) {
            render_directory_archive_unsupported(&mut self.response, head_only)?;
        } else if target.is_dir {
            if self.query_params.contains_key("q") {
                self.server
                    .handle_search_dir(
                        target.path.as_path(),
                        &self.query_params,
                        head_only,
                        session.clone(),
                        &mut self.response,
                    )
                    .await?;
            } else {
                self.server
                    .handle_ls_dir(
                        target.path.as_path(),
                        true,
                        &self.query_params,
                        head_only,
                        session.clone(),
                        &mut self.response,
                    )
                    .await?;
            }
        } else if target.is_file {
            self.server
                .handle_send_file(
                    target.path.as_path(),
                    &self.headers,
                    head_only,
                    &mut self.response,
                )
                .await?;
        } else if self.request_path.ends_with('/') {
            self.server
                .handle_ls_dir(
                    target.path.as_path(),
                    false,
                    &self.query_params,
                    head_only,
                    session.clone(),
                    &mut self.response,
                )
                .await?;
        } else {
            status_not_found(&mut self.response);
        }
        Ok(())
    }

    async fn dispatch_fresh_upload(
        &mut self,
        target: &mut PreparedTarget,
        headers: MutationHeaders,
        session: &SessionInfo,
    ) -> Result<()> {
        let upload = headers.upload.expect("PUT headers were parsed");
        debug_assert_eq!(upload.mode, UploadMode::Fresh);
        match timeout_at(
            target
                .upload_deadline
                .expect("upload has an upload deadline"),
            self.server
                .has_persisted_path_descendant(target.path.as_path()),
        )
        .await
        {
            Ok(Ok(false)) => {}
            Ok(Ok(true)) => {
                return self.render_upload_rejection(
                    upload,
                    StatusCode::CONFLICT,
                    ErrorCode::UPLOAD_STATE_CONFLICT,
                    "Upload target conflicts with an active upload or pending delete",
                    RecoveryAdvice::RefreshTarget,
                );
            }
            Ok(Err(error)) => {
                log::error!("Failed to inspect durable state before upload error={error:#}");
                return self.render_upload_rejection(
                    upload,
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::UPLOAD_STATE_UNAVAILABLE,
                    "Upload safety state is temporarily unavailable",
                    RecoveryAdvice::RetryAfterSeconds(1),
                );
            }
            Err(_) => {
                return self.render_upload_rejection(
                    upload,
                    StatusCode::REQUEST_TIMEOUT,
                    ErrorCode::REQUEST_TIMEOUT,
                    "Upload timed out while inspecting durable safety state",
                    RecoveryAdvice::Retry,
                );
            }
        }
        self.run_upload(target, upload, session).await
    }

    async fn dispatch_resumed_upload(
        &mut self,
        target: &mut PreparedTarget,
        headers: MutationHeaders,
        session: &SessionInfo,
    ) -> Result<()> {
        let upload = headers.upload.expect("PATCH headers were parsed");
        debug_assert!(upload.mode.is_resume());
        self.run_upload(target, upload, session).await
    }

    async fn run_upload(
        &mut self,
        target: &mut PreparedTarget,
        upload: UploadRequest,
        session: &SessionInfo,
    ) -> Result<()> {
        let options = UploadOptions {
            owner: session.user.clone(),
            mode: upload.mode,
            upload_id: upload.id,
            upload_length: upload.length,
            overwrite: upload.overwrite,
            deadline: target
                .upload_deadline
                .expect("upload has an upload deadline"),
            path_lease: target
                .path_lease
                .take()
                .expect("upload acquired a path lease"),
        };
        let request = self.take_request();
        self.response = self
            .server
            .run_tracked_upload(
                target.path.as_path(),
                options,
                request,
                target
                    .upload_permit
                    .take()
                    .expect("upload acquired an upload permit"),
            )
            .await?;
        Ok(())
    }

    async fn dispatch_delete(
        &mut self,
        target: &mut PreparedTarget,
        headers: MutationHeaders,
        session: &SessionInfo,
    ) -> Result<()> {
        let (id, operation) = target
            .delete_operation
            .take()
            .expect("DELETE operation was started");
        let expected_revision = match headers.delete_condition {
            DeleteCondition::Revision(revision) => revision,
            _ => unreachable!("DELETE revision was validated before the operation began"),
        };
        let revision_owner = OwnerId::persistent(&session.user);
        let identity = self
            .server
            .content
            .rooted_fs
            .replacement_identity(target.path.as_path())
            .await?;
        let relative = self
            .server
            .content
            .rooted_fs
            .state_relative_path(target.path.as_path())?;
        let current_revision = target_revision(revision_owner, &relative, identity);
        if current_revision != Some(expected_revision) {
            apply_revision_header(&mut self.response, SOURCE_REVISION_HEADER, current_revision)?;
            let outcome = OperationOutcome::failure(
                StatusCode::PRECONDITION_FAILED,
                TrackedOperationError::DeleteTargetChanged,
            );
            operation.complete(outcome).await?;
            return apply_operation_outcome(&mut self.response, id, outcome, false);
        }
        let delete_identity = identity
            .delete_identity()
            .expect("a matching DELETE revision identifies an existing target");
        self.server
            .handle_delete(
                DeleteRequest {
                    owner: &session.user,
                    path: target.path.as_path(),
                    expected_revision_identity: identity,
                    expected_delete_identity: delete_identity,
                    mutation: self.mutation.clone(),
                    path_lease: target
                        .path_lease
                        .take()
                        .expect("DELETE acquired a path lease"),
                    operation: (id, operation),
                },
                &mut self.response,
            )
            .await
    }

    fn render_upload_rejection(
        &mut self,
        upload: UploadRequest,
        status: StatusCode,
        code: ErrorCode,
        detail: &'static str,
        recovery: RecoveryAdvice,
    ) -> Result<()> {
        render_upload_problem(
            &mut self.response,
            status,
            code,
            detail,
            recovery,
            upload.id,
            upload.length,
            upload.mode.offset(),
            UploadPublicState::NotStarted,
        )
    }

    fn take_request(&mut self) -> Request {
        self.request
            .take()
            .expect("request body was not consumed by an earlier route phase")
    }
}

fn parse_delete_condition(headers: &HeaderMap) -> DeleteCondition {
    let mut values = headers.get_all(IF_MATCH).iter();
    let Some(value) = values.next() else {
        return DeleteCondition::Missing;
    };
    if values.next().is_some() {
        return DeleteCondition::Invalid;
    }
    let Ok(value) = value.to_str() else {
        return DeleteCondition::Invalid;
    };
    let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return DeleteCondition::Invalid;
    };
    TargetRevision::parse(value)
        .map(DeleteCondition::Revision)
        .unwrap_or(DeleteCondition::Invalid)
}

fn render_api_method_not_allowed(res: &mut Response, allow: &'static str) -> Result<()> {
    status_method_not_allowed(res, allow);
    render_problem(
        res,
        &ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::METHOD_NOT_ALLOWED,
            "Method not allowed for this API endpoint",
        ),
    )
}

/// Keeps the former directory-archive route explicit during the compatibility
/// window. Treat every value as the retired feature so `?zip=1` cannot
/// silently fall through to an HTML listing while `?zip` returns a problem.
fn directory_archive_requested(query_params: &HashMap<String, String>) -> bool {
    query_params.contains_key("zip")
}

fn render_directory_archive_unsupported(res: &mut Response, head_only: bool) -> Result<()> {
    render_problem(
        res,
        &ApiError::new(
            StatusCode::GONE,
            ErrorCode::DIRECTORY_ARCHIVE_UNSUPPORTED,
            "Directory archive downloads are no longer supported",
        ),
    )?;
    if head_only {
        *res.body_mut() = body_full("");
    }
    Ok(())
}

fn apply_tracked_delete_rejection(
    operation_id: uuid::Uuid,
    res: &mut Response,
    status: StatusCode,
    code: ErrorCode,
    message: &'static str,
) -> Result<()> {
    set_operation_headers(res, operation_id, OperationPublicState::Rejected);
    render_problem(
        res,
        &ApiError::new(status, code, message).with_operation(OperationProblemContext::new(
            operation_id.hyphenated().to_string(),
            OperationPublicState::Rejected,
            None,
        )),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Args, auth::AuthConfig};
    use futures_util::poll;
    use http_body_util::BodyExt as _;
    use hyper::header::CONTENT_TYPE;
    use serde_json::json;
    use std::{
        net::{Ipv4Addr, SocketAddr},
        os::unix::fs::PermissionsExt,
        path::Path,
        task::Poll,
    };
    use uuid::Uuid;

    const TEST_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

    fn upload_test_server(root: &Path) -> (Arc<Server>, assert_fs::TempDir) {
        let state_dir = assert_fs::TempDir::new().unwrap();
        std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = Server::init_with_lifecycle(
            Args {
                serve_path: root.to_path_buf(),
                state_dir: Some(state_dir.path().to_path_buf()),
                auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
                max_concurrent_uploads: 1,
                ..Args::default()
            },
            super::super::super::ServerLifecycle::new(),
        )
        .unwrap();
        (Arc::new(server), state_dir)
    }

    #[test]
    fn every_zip_query_value_selects_the_retired_archive_route() {
        for value in ["", "1", "store"] {
            let query = HashMap::from([("zip".to_string(), value.to_string())]);
            assert!(directory_archive_requested(&query));
        }
        assert!(!directory_archive_requested(&HashMap::new()));
    }

    #[tokio::test]
    async fn retired_directory_archive_get_returns_a_typed_gone_problem() {
        let mut response = Response::default();
        render_directory_archive_unsupported(&mut response, false).unwrap();

        assert_eq!(response.status(), StatusCode::GONE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({
                "type": "urn:dufs:problem:directory_archive_unsupported",
                "title": "Gone",
                "status": 410,
                "detail": "Directory archive downloads are no longer supported",
                "code": "directory_archive_unsupported"
            })
        );
    }

    #[tokio::test]
    async fn retired_directory_archive_head_has_problem_headers_and_no_body() {
        let mut response = Response::default();
        render_directory_archive_unsupported(&mut response, true).unwrap();

        assert_eq!(response.status(), StatusCode::GONE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn fresh_upload_state_check_obeys_deadline_and_releases_admission() {
        let root = assert_fs::TempDir::new().unwrap();
        let (server, _state_dir) = upload_test_server(root.path());
        let release_actor = server.state.state_store.block_actor_for_test().unwrap();
        let route_path = server
            .content
            .path_policy
            .parse_route("/deadline.bin")
            .unwrap();
        let target_path = server
            .content
            .path_policy
            .parse_browser_target("/deadline.bin")
            .unwrap();
        let upload_id = Uuid::new_v4();
        let upload = UploadRequest {
            id: upload_id,
            length: 7,
            mode: UploadMode::Fresh,
            overwrite: UploadOverwritePolicy::NoReplace,
        };
        let mut target = PreparedTarget {
            path: target_path.clone(),
            is_miss: true,
            miss_is_hidden: false,
            is_dir: false,
            is_file: false,
            delete_operation: None,
            path_lease: Some(
                server
                    .content
                    .path_coordinator
                    .acquire([target_path.as_path()])
                    .await,
            ),
            upload_permit: Some(
                server
                    .admission
                    .upload_slots
                    .clone()
                    .try_acquire_owned()
                    .unwrap(),
            ),
            upload_deadline: Some(Instant::now() + Duration::from_millis(100)),
        };
        let mut context = RequestContext::for_test(SocketAddr::from((Ipv4Addr::LOCALHOST, 1)));
        let mut dispatcher = RequestDispatcher {
            server: server.clone(),
            request: None,
            route_path,
            internal_api_request: true,
            mutation: MutationProgress::default(),
            context: &mut context,
            method: Method::PUT,
            uri: Uri::from_static("/deadline.bin"),
            headers: HeaderMap::new(),
            request_path: "/deadline.bin".to_owned(),
            query_params: HashMap::new(),
            response: Response::default(),
        };
        let session = SessionInfo {
            user: "user".to_owned(),
            csrf_token: String::new(),
        };

        let mut dispatch = Box::pin(dispatcher.dispatch_fresh_upload(
            &mut target,
            MutationHeaders {
                upload: Some(upload),
                ..MutationHeaders::default()
            },
            &session,
        ));
        assert!(
            matches!(poll!(dispatch.as_mut()), Poll::Pending),
            "the durable-state query did not wait behind the blocked actor"
        );
        dispatch.as_mut().await.unwrap();
        drop(dispatch);
        let response = std::mem::take(&mut dispatcher.response);
        drop(dispatcher);
        drop(target);

        let replacement_permit = server
            .admission
            .upload_slots
            .clone()
            .try_acquire_owned()
            .expect("the timed-out upload retained its global permit");
        let replacement_lease = tokio::time::timeout(
            Duration::from_secs(1),
            server
                .content
                .path_coordinator
                .acquire([target_path.as_path()]),
        )
        .await
        .expect("the timed-out upload retained its path lease");

        release_actor.send(()).unwrap();
        server.state.state_store.probe_readiness().await.unwrap();
        drop(replacement_lease);
        drop(replacement_permit);

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(
            response.headers()["x-dufs-upload-id"].to_str().unwrap(),
            upload_id.to_string()
        );
        assert_eq!(response.headers()["x-dufs-upload-length"], "7");
        assert_eq!(response.headers()["x-dufs-operation-state"], "not-started");
        assert!(!response.headers().contains_key("x-dufs-upload-offset"));
        assert!(!response.headers().contains_key("retry-after"));
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({
                "type": "urn:dufs:problem:request_timeout",
                "title": "Request Timeout",
                "status": 408,
                "detail": "Upload timed out while inspecting durable safety state",
                "code": "request_timeout",
                "recovery": "retry",
                "upload_id": upload_id.to_string(),
                "upload_state": "not-started",
                "upload_length": 7
            })
        );
        assert!(!root.path().join("deadline.bin").exists());
    }
}
