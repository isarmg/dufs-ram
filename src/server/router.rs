use super::{
    Request, Response, Server, apply_app_error,
    operation_registry::{
        OPERATION_ID_HEADER, OPERATION_STATE_HEADER, OperationOutcome, apply_operation_outcome,
        apply_unknown, set_operation_headers,
    },
    problem::{ApiError, ErrorCode, OperationProblemContext, RecoveryAdvice, render_problem},
    protocol::{OperationPublicState, UploadPublicState},
    set_private_no_store, status_error, upload,
};
use crate::{app_error::AppError, request_context::RequestContext};

use anyhow::Result;
use hyper::{Method, StatusCode};
use std::{net::SocketAddr, sync::Arc, time::Duration};

mod dispatch;
mod request;

pub(in crate::server) use request::MutationProgress;
use request::RequestProfile;

impl Server {
    pub async fn call(
        self: Arc<Self>,
        req: Request,
        addr: SocketAddr,
    ) -> Result<Response, hyper::Error> {
        let relative_path = self.resolve_path(req.uri().path());
        let public_asset_request = matches!(req.method(), &Method::GET | &Method::HEAD)
            && relative_path
                .as_deref()
                .is_some_and(|path| self.is_public_asset_path(path));
        let profile = RequestProfile::new(&req, relative_path.as_deref(), public_asset_request);
        let mutation = profile.mutation();

        // An embedder may retain an Arc<Server> after ServerRuntime shutdown.
        // Reject new work before routing so closed trackers and a stopped
        // maintenance worker cannot acquire fresh filesystem obligations.
        let Some(_request_guard) = self.lifecycle.enter_request().await else {
            let mut res = Response::default();
            if let Some(operation_id) = profile.operation_id() {
                set_operation_headers(&mut res, operation_id, OperationPublicState::Rejected);
                render_problem(
                    &mut res,
                    &ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorCode::SERVER_STOPPING,
                        "Server is shutting down",
                    )
                    .with_recovery(RecoveryAdvice::RetryAfterSeconds(1))
                    .with_operation(OperationProblemContext::new(
                        operation_id.hyphenated().to_string(),
                        OperationPublicState::Rejected,
                        None,
                    )),
                )
                .expect("serializing a fixed operation problem cannot fail");
            } else if let Some(upload) = profile.upload_context() {
                upload::apply_upload_problem(
                    &mut res,
                    upload::UploadErrorContext::new(
                        upload.id,
                        UploadPublicState::NotStarted,
                        Some(upload.length),
                        upload.offset,
                    ),
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::SERVER_STOPPING,
                    "Server is shutting down",
                    RecoveryAdvice::RetryAfterSeconds(1),
                )
                .expect("serializing a fixed upload problem cannot fail");
            } else if profile.is_internal_api() {
                render_problem(
                    &mut res,
                    &ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorCode::SERVER_STOPPING,
                        "Server is shutting down",
                    )
                    .with_recovery(RecoveryAdvice::RetryAfterSeconds(1)),
                )
                .expect("serializing a fixed problem response cannot fail");
            } else {
                status_error(
                    &mut res,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Server is shutting down",
                );
            }
            set_private_no_store(&mut res);
            return Ok(res);
        };
        let mut context = RequestContext::new(&req, addr, &self.content.args.http_logger);
        if let Some(operation_id) = profile.operation_id() {
            self.content.args.http_logger.set_runtime_value(
                context.access_log_mut(),
                "operation_id",
                || operation_id.hyphenated().to_string(),
            );
        }
        let handle = self.clone().handle_inner(
            req,
            relative_path,
            profile.is_internal_api(),
            mutation.clone(),
            &mut context,
        );
        let handle_result = if profile.is_upload() {
            handle.await
        } else {
            match tokio::time::timeout(
                Duration::from_secs(self.content.args.request_timeout),
                handle,
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    let mut res = Response::default();
                    if let Some(operation_id) = profile.operation_id() {
                        apply_operation_timeout(&mut res, operation_id, &mutation)
                            .expect("serializing a fixed operation response cannot fail");
                    } else if profile.is_internal_api() {
                        render_problem(
                            &mut res,
                            &ApiError::new(
                                StatusCode::GATEWAY_TIMEOUT,
                                ErrorCode::REQUEST_TIMEOUT,
                                "Request timed out",
                            )
                            .with_recovery(RecoveryAdvice::Retry),
                        )
                        .expect("serializing a fixed problem response cannot fail");
                    } else {
                        status_error(&mut res, StatusCode::GATEWAY_TIMEOUT, "Request timed out");
                    }
                    self.content.args.http_logger.set_runtime_value(
                        context.access_log_mut(),
                        "status",
                        || StatusCode::GATEWAY_TIMEOUT.as_u16().to_string(),
                    );
                    if profile.operation_id().is_some() {
                        let state = if mutation.outcome_can_be_unknown() {
                            OperationPublicState::Unknown
                        } else {
                            OperationPublicState::Rejected
                        };
                        self.content.args.http_logger.set_runtime_value(
                            context.access_log_mut(),
                            "operation_state",
                            || state.wire_name().to_owned(),
                        );
                    }
                    self.content.args.http_logger.log(
                        context.access_log(),
                        Some("request time budget exceeded".to_string()),
                    );
                    set_private_no_store(&mut res);
                    return Ok(res);
                }
            }
        };

        let (mut res, successful_public_asset) = match handle_result {
            Ok(mut res) => {
                if let Some(operation_id) = profile.operation_id()
                    && !res.headers().contains_key(OPERATION_ID_HEADER)
                {
                    let state = if res.status().is_success() {
                        OperationPublicState::Succeeded
                    } else if mutation.outcome_can_be_unknown() {
                        OperationPublicState::Unknown
                    } else {
                        OperationPublicState::Rejected
                    };
                    set_operation_headers(&mut res, operation_id, state);
                }
                if let Some(operation_state) = res
                    .headers()
                    .get(OPERATION_STATE_HEADER)
                    .and_then(|value| value.to_str().ok())
                {
                    self.content.args.http_logger.set_runtime_value(
                        context.access_log_mut(),
                        "operation_state",
                        || operation_state.to_string(),
                    );
                }
                let successful_public_asset =
                    profile.is_public_asset() && res.status() == StatusCode::OK;
                self.content.args.http_logger.set_runtime_value(
                    context.access_log_mut(),
                    "status",
                    || res.status().as_u16().to_string(),
                );
                if !(profile.omit_success_log() && res.status() == StatusCode::OK) {
                    self.content
                        .args
                        .http_logger
                        .log(context.access_log(), None);
                }
                (res, successful_public_asset)
            }
            Err(err) => {
                let mut res = Response::default();
                let error = AppError::from_anyhow(err);
                if let Some(operation_id) = profile.operation_id() {
                    if mutation.outcome_can_be_unknown() {
                        apply_operation_outcome(
                            &mut res,
                            operation_id,
                            OperationOutcome::uncertain(),
                            false,
                        )
                        .expect("serializing a fixed operation response cannot fail");
                    } else {
                        apply_operation_failure_before_commit(&mut res, operation_id)
                            .expect("serializing a fixed operation response cannot fail");
                    }
                    let state = if mutation.outcome_can_be_unknown() {
                        OperationPublicState::Unknown
                    } else {
                        OperationPublicState::Rejected
                    };
                    self.content.args.http_logger.set_runtime_value(
                        context.access_log_mut(),
                        "operation_state",
                        || state.wire_name().to_owned(),
                    );
                } else if let Some(upload) = profile.upload_context() {
                    let status = if error.status() == StatusCode::GATEWAY_TIMEOUT {
                        StatusCode::GATEWAY_TIMEOUT
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    };
                    render_upload_problem(
                        &mut res,
                        status,
                        ErrorCode::UPLOAD_RESULT_UNKNOWN,
                        "Upload result could not be confirmed",
                        RecoveryAdvice::QueryUpload,
                        upload.id,
                        upload.length,
                        upload.offset,
                        UploadPublicState::Unknown,
                    )
                    .expect("serializing a validated upload problem cannot fail");
                } else if profile.is_internal_api() {
                    render_problem(&mut res, &api_error_from_app_error(&error))
                        .expect("serializing a validated API problem cannot fail");
                } else {
                    apply_app_error(&mut res, &error);
                }
                self.content.args.http_logger.set_runtime_value(
                    context.access_log_mut(),
                    "status",
                    || res.status().as_u16().to_string(),
                );
                self.content
                    .args
                    .http_logger
                    .log(context.access_log(), Some(error.to_string()));
                (res, false)
            }
        };
        if !successful_public_asset {
            set_private_no_store(&mut res);
        }

        Ok(res)
    }
}

#[allow(clippy::too_many_arguments)]
fn render_upload_problem(
    res: &mut Response,
    status: StatusCode,
    code: ErrorCode,
    detail: impl Into<std::borrow::Cow<'static, str>>,
    recovery: RecoveryAdvice,
    upload_id: uuid::Uuid,
    upload_length: u64,
    upload_offset: Option<u64>,
    state: UploadPublicState,
) -> Result<()> {
    upload::apply_upload_problem(
        res,
        upload::UploadErrorContext::new(upload_id, state, Some(upload_length), upload_offset),
        status,
        code,
        detail,
        recovery,
    )
}

fn api_error_from_app_error(error: &AppError) -> ApiError {
    let code = match error.status() {
        StatusCode::BAD_REQUEST => ErrorCode::INVALID_REQUEST,
        StatusCode::FORBIDDEN => ErrorCode::REQUEST_FORBIDDEN,
        StatusCode::NOT_FOUND => ErrorCode::REQUEST_NOT_FOUND,
        StatusCode::CONFLICT => ErrorCode::REQUEST_CONFLICT,
        StatusCode::GATEWAY_TIMEOUT => ErrorCode::FILESYSTEM_TIMEOUT,
        StatusCode::INSUFFICIENT_STORAGE => ErrorCode::INSUFFICIENT_STORAGE,
        _ => ErrorCode::INTERNAL_ERROR,
    };
    let recovery = if matches!(
        error.status(),
        StatusCode::GATEWAY_TIMEOUT | StatusCode::SERVICE_UNAVAILABLE
    ) {
        RecoveryAdvice::Retry
    } else {
        RecoveryAdvice::None
    };
    ApiError::new(error.status(), code, error.public_message().to_owned()).with_recovery(recovery)
}

fn apply_operation_timeout(
    res: &mut Response,
    operation_id: uuid::Uuid,
    mutation: &MutationProgress,
) -> Result<()> {
    if mutation.outcome_can_be_unknown() {
        return apply_unknown(
            res,
            operation_id,
            StatusCode::GATEWAY_TIMEOUT,
            ErrorCode::OPERATION_RESULT_UNKNOWN,
            "Request timed out after commit dispatch; query the operation status before retrying",
        );
    }

    apply_operation_rejected(
        res,
        operation_id,
        StatusCode::GATEWAY_TIMEOUT,
        ErrorCode::REQUEST_TIMEOUT,
        "Request timed out before filesystem commit; retrying with the same operation ID is safe",
        RecoveryAdvice::Retry,
    )
}

fn apply_operation_failure_before_commit(
    res: &mut Response,
    operation_id: uuid::Uuid,
) -> Result<()> {
    apply_operation_rejected(
        res,
        operation_id,
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::OPERATION_NOT_COMMITTED,
        "Request failed before filesystem commit; retrying with the same operation ID is safe",
        RecoveryAdvice::Retry,
    )
}

fn apply_operation_rejected(
    res: &mut Response,
    operation_id: uuid::Uuid,
    status: StatusCode,
    code: ErrorCode,
    detail: &'static str,
    recovery: RecoveryAdvice,
) -> Result<()> {
    set_operation_headers(res, operation_id, OperationPublicState::Rejected);
    render_problem(
        res,
        &ApiError::new(status, code, detail)
            .with_recovery(recovery)
            .with_operation(OperationProblemContext::new(
                operation_id.hyphenated().to_string(),
                OperationPublicState::Rejected,
                None,
            )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    async fn timeout_problem(progress: &MutationProgress) -> (Response, serde_json::Value) {
        let mut response = Response::default();
        apply_operation_timeout(&mut response, uuid::Uuid::new_v4(), progress).unwrap();
        let body = response.body_mut().collect().await.unwrap().to_bytes();
        let problem = serde_json::from_slice(&body).unwrap();
        (response, problem)
    }

    #[tokio::test]
    async fn precommit_timeout_is_a_retryable_rejection() {
        let progress = MutationProgress::default();
        progress.mark_reserved();

        let (response, problem) = timeout_problem(&progress).await;

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(response.headers()[OPERATION_STATE_HEADER], "rejected");
        assert_eq!(problem["code"], "request_timeout");
        assert_eq!(problem["recovery"], "retry");
    }

    #[tokio::test]
    async fn detached_commit_timeout_requires_status_query() {
        let progress = MutationProgress::default();
        progress.mark_reserved();
        progress.mark_detached_commit();

        let (response, problem) = timeout_problem(&progress).await;

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(response.headers()[OPERATION_STATE_HEADER], "unknown");
        assert_eq!(problem["code"], "operation_result_unknown");
        assert_eq!(problem["recovery"], "query_job");
    }
}
