use super::{
    Response, body_full,
    identity::OwnerId,
    problem::{ApiError, ErrorCode, OperationProblemContext, RecoveryAdvice, render_problem},
    protocol::OperationPublicState,
    state_store::{
        OperationKey, StateStore, StateStoreDispatchError, StoreBegin, StoreStatus, StoredOutcome,
        StoredTerminalState,
    },
};
use headers::{ContentType, HeaderMapExt};
use hyper::{
    StatusCode,
    header::{HeaderMap, HeaderValue},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use uuid::Uuid;

pub(super) const OPERATION_ID_HEADER: &str = "x-dufs-operation-id";
pub(super) const OPERATION_STATE_HEADER: &str = "x-dufs-operation-state";
pub(super) const JOB_STATUS_PREFIX: &str = "__dufs__/api/jobs/";

const REGISTRY_CAPACITY: usize = 4096;
const PER_OWNER_CAPACITY: usize = 1024;
const RESULT_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub(super) struct OperationRegistry {
    store: StateStore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OperationFingerprint([u8; 32]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OperationOutcome {
    status: StatusCode,
    state: OperationPublicState,
    error: Option<TrackedOperationError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TrackedOperationError {
    InvalidJson,
    InvalidPath,
    PathExists,
    InvalidSourcePath,
    InvalidDestinationPath,
    SourceEqualsDestination,
    InvalidMovePath,
    InvalidRenameName,
    InvalidRenamePath,
    SourceNotFound,
    DirectoryIntoItself,
    DestinationExists,
    DestinationDirectoryNotFound,
    DestinationNotDirectory,
    DirectoryOverwriteForbidden,
    MoveStateConflict,
    RenameStateConflict,
    TargetNotFound,
    DeleteStateConflict,
    PurgeBacklogFull,
    PurgeStateUnavailable,
    DeleteTargetChanged,
    DeleteNotCommitted,
    OutcomeUncertain,
}

pub(super) enum BeginOperation {
    Started(OperationGuard),
    Running,
    Replay(OperationOutcome),
    Conflict,
    Full,
    Unavailable,
}

pub(super) enum OperationStatus {
    Running,
    Completed(OperationOutcome),
    NotFound,
    Unavailable,
}

pub(super) struct OperationGuard {
    registry: OperationRegistry,
    key: OperationKey,
    lease: [u8; 16],
    completed: bool,
}

#[derive(Serialize)]
struct OperationBody<'a> {
    operation_id: String,
    state: OperationPublicState,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

#[derive(Serialize)]
struct JobBody<'a> {
    job_id: String,
    state: OperationPublicState,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

impl OperationRegistry {
    #[cfg(test)]
    fn with_limits(capacity: usize, per_owner_capacity: usize, result_ttl: Duration) -> Self {
        assert!(capacity > 0, "operation registry capacity must be positive");
        assert!(
            (1..=capacity).contains(&per_owner_capacity),
            "per-owner operation capacity must be positive and no greater than global capacity"
        );
        let store = StateStore::temporary_for_test(capacity, per_owner_capacity, result_ttl)
            .expect("a temporary file-backed SQLite operation store must initialize");
        Self { store }
    }

    #[cfg(test)]
    fn temporary_for_test() -> anyhow::Result<Self> {
        Ok(Self {
            store: StateStore::temporary_for_test(
                REGISTRY_CAPACITY,
                PER_OWNER_CAPACITY,
                RESULT_TTL,
            )?,
        })
    }

    pub(super) fn open(
        path: &Path,
        root_identity: super::state_store::RootIdentity,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            store: StateStore::open(
                path,
                &root_identity,
                REGISTRY_CAPACITY,
                PER_OWNER_CAPACITY,
                RESULT_TTL,
            )?,
        })
    }

    pub(super) fn is_healthy(&self) -> bool {
        self.store.is_healthy()
    }

    pub(super) fn state_store(&self) -> StateStore {
        self.store.clone()
    }

    pub(super) async fn begin(
        &self,
        owner: &str,
        id: Uuid,
        fingerprint: OperationFingerprint,
    ) -> anyhow::Result<BeginOperation> {
        let key = OperationKey::new(owner, id);
        let stored = match self.store.begin_operation(key, fingerprint.0).await {
            Ok(stored) => stored,
            Err(error) if error.downcast_ref::<StateStoreDispatchError>().is_some() => {
                return Ok(BeginOperation::Unavailable);
            }
            Err(error) => return Err(error),
        };
        Ok(match stored {
            StoreBegin::Started { lease } => BeginOperation::Started(OperationGuard {
                registry: self.clone(),
                key,
                lease,
                completed: false,
            }),
            StoreBegin::Running => BeginOperation::Running,
            StoreBegin::Replay(outcome) => {
                BeginOperation::Replay(OperationOutcome::from_stored(outcome)?)
            }
            StoreBegin::Conflict => BeginOperation::Conflict,
            StoreBegin::Full => BeginOperation::Full,
        })
    }

    pub(super) async fn status(&self, owner: &str, id: Uuid) -> anyhow::Result<OperationStatus> {
        let key = OperationKey::new(owner, id);
        let stored = match self.store.operation_status(key).await {
            Ok(stored) => stored,
            Err(error) => {
                log::error!(
                    "Failed to query operation state operation_id={} error={error:#}",
                    id
                );
                return Ok(OperationStatus::Unavailable);
            }
        };
        Ok(match stored {
            StoreStatus::Running => OperationStatus::Running,
            StoreStatus::Completed(outcome) => {
                OperationStatus::Completed(OperationOutcome::from_stored(outcome)?)
            }
            StoreStatus::NotFound => OperationStatus::NotFound,
        })
    }
}

impl OperationKey {
    fn new(owner: &str, id: Uuid) -> Self {
        Self {
            owner: OwnerId::persistent(owner).into_bytes(),
            id: *id.as_bytes(),
        }
    }
}

impl OperationFingerprint {
    pub(super) fn new(parts: &[&[u8]]) -> Self {
        let mut digest = Sha256::new();
        for part in parts {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part);
        }
        Self(digest.finalize().into())
    }
}

impl OperationOutcome {
    pub(super) fn success(status: StatusCode) -> Self {
        debug_assert!(status.is_success());
        Self {
            status,
            state: OperationPublicState::Succeeded,
            error: None,
        }
    }

    pub(super) fn failure(status: StatusCode, error: TrackedOperationError) -> Self {
        debug_assert!(!status.is_success());
        Self {
            status,
            state: OperationPublicState::Failed,
            error: Some(error),
        }
    }

    pub(super) fn uncertain() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            state: OperationPublicState::Unknown,
            error: Some(TrackedOperationError::OutcomeUncertain),
        }
    }

    const fn public_state(self) -> OperationPublicState {
        self.state
    }

    fn into_stored(self) -> StoredOutcome {
        StoredOutcome {
            status: self.status.as_u16(),
            state: match self.state {
                OperationPublicState::Succeeded => StoredTerminalState::Succeeded,
                OperationPublicState::Failed => StoredTerminalState::Failed,
                OperationPublicState::Unknown => StoredTerminalState::Unknown,
                OperationPublicState::Running | OperationPublicState::Rejected => {
                    unreachable!("an operation outcome must be terminal")
                }
            },
            code: self.error.map(|error| error.code().as_str().to_owned()),
        }
    }

    fn from_stored(outcome: StoredOutcome) -> anyhow::Result<Self> {
        let status = StatusCode::from_u16(outcome.status)
            .map_err(|_| anyhow::anyhow!("operation store contains an invalid HTTP status"))?;
        let state = match outcome.state {
            StoredTerminalState::Succeeded => OperationPublicState::Succeeded,
            StoredTerminalState::Failed => OperationPublicState::Failed,
            StoredTerminalState::Unknown => OperationPublicState::Unknown,
        };
        let error = match outcome.code.as_deref() {
            None if state == OperationPublicState::Succeeded => None,
            Some(code) if state != OperationPublicState::Succeeded => {
                let Some(error) = TrackedOperationError::from_wire_name(code) else {
                    anyhow::bail!("operation store contains an unknown terminal error");
                };
                Some(error)
            }
            _ => anyhow::bail!("operation store contains an incomplete terminal outcome"),
        };
        if state == OperationPublicState::Succeeded && !status.is_success() {
            anyhow::bail!("operation store contains a non-success status for a success outcome");
        }
        if state != OperationPublicState::Succeeded && status.is_success() {
            anyhow::bail!("operation store contains a success status for an error outcome");
        }
        Ok(Self {
            status,
            state,
            error,
        })
    }
}

impl TrackedOperationError {
    const fn from_wire_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"invalid_json" => Some(Self::InvalidJson),
            b"invalid_path" => Some(Self::InvalidPath),
            b"path_exists" => Some(Self::PathExists),
            b"invalid_source_path" => Some(Self::InvalidSourcePath),
            b"invalid_destination_path" => Some(Self::InvalidDestinationPath),
            b"source_equals_destination" => Some(Self::SourceEqualsDestination),
            b"invalid_move_path" => Some(Self::InvalidMovePath),
            b"invalid_rename_name" => Some(Self::InvalidRenameName),
            b"invalid_rename_path" => Some(Self::InvalidRenamePath),
            b"source_not_found" => Some(Self::SourceNotFound),
            b"directory_into_itself" => Some(Self::DirectoryIntoItself),
            b"destination_exists" => Some(Self::DestinationExists),
            b"destination_directory_not_found" => Some(Self::DestinationDirectoryNotFound),
            b"destination_not_directory" => Some(Self::DestinationNotDirectory),
            b"directory_overwrite_forbidden" => Some(Self::DirectoryOverwriteForbidden),
            b"move_state_conflict" => Some(Self::MoveStateConflict),
            b"rename_state_conflict" => Some(Self::RenameStateConflict),
            b"target_not_found" => Some(Self::TargetNotFound),
            b"delete_state_conflict" => Some(Self::DeleteStateConflict),
            b"purge_backlog_full" => Some(Self::PurgeBacklogFull),
            b"purge_state_unavailable" => Some(Self::PurgeStateUnavailable),
            b"delete_target_changed" => Some(Self::DeleteTargetChanged),
            b"delete_not_committed" => Some(Self::DeleteNotCommitted),
            b"outcome_uncertain" => Some(Self::OutcomeUncertain),
            _ => None,
        }
    }

    pub(super) const fn code(self) -> ErrorCode {
        match self {
            Self::InvalidJson => ErrorCode::INVALID_JSON,
            Self::InvalidPath => ErrorCode::INVALID_PATH,
            Self::PathExists => ErrorCode::PATH_EXISTS,
            Self::InvalidSourcePath => ErrorCode::INVALID_SOURCE_PATH,
            Self::InvalidDestinationPath => ErrorCode::INVALID_DESTINATION_PATH,
            Self::SourceEqualsDestination => ErrorCode::SOURCE_EQUALS_DESTINATION,
            Self::InvalidMovePath => ErrorCode::INVALID_MOVE_PATH,
            Self::InvalidRenameName => ErrorCode::INVALID_RENAME_NAME,
            Self::InvalidRenamePath => ErrorCode::INVALID_RENAME_PATH,
            Self::SourceNotFound => ErrorCode::SOURCE_NOT_FOUND,
            Self::DirectoryIntoItself => ErrorCode::DIRECTORY_INTO_ITSELF,
            Self::DestinationExists => ErrorCode::DESTINATION_EXISTS,
            Self::DestinationDirectoryNotFound => ErrorCode::DESTINATION_DIRECTORY_NOT_FOUND,
            Self::DestinationNotDirectory => ErrorCode::DESTINATION_NOT_DIRECTORY,
            Self::DirectoryOverwriteForbidden => ErrorCode::DIRECTORY_OVERWRITE_FORBIDDEN,
            Self::MoveStateConflict => ErrorCode::MOVE_STATE_CONFLICT,
            Self::RenameStateConflict => ErrorCode::RENAME_STATE_CONFLICT,
            Self::TargetNotFound => ErrorCode::TARGET_NOT_FOUND,
            Self::DeleteStateConflict => ErrorCode::DELETE_STATE_CONFLICT,
            Self::PurgeBacklogFull => ErrorCode::PURGE_BACKLOG_FULL,
            Self::PurgeStateUnavailable => ErrorCode::PURGE_STATE_UNAVAILABLE,
            Self::DeleteTargetChanged => ErrorCode::DELETE_TARGET_CHANGED,
            Self::DeleteNotCommitted => ErrorCode::DELETE_NOT_COMMITTED,
            Self::OutcomeUncertain => ErrorCode::OUTCOME_UNCERTAIN,
        }
    }

    pub(super) const fn detail(self) -> &'static str {
        match self {
            Self::InvalidJson => "Invalid JSON request",
            Self::InvalidPath => "Invalid path",
            Self::PathExists => "Path already exists",
            Self::InvalidSourcePath => "Invalid source path",
            Self::InvalidDestinationPath => "Invalid destination path",
            Self::SourceEqualsDestination => "Source and destination must differ",
            Self::InvalidMovePath => "Invalid move path",
            Self::InvalidRenameName => "Rename name must be one valid path segment",
            Self::InvalidRenamePath => "Invalid rename path",
            Self::SourceNotFound => "Source not found",
            Self::DirectoryIntoItself => "A directory cannot be moved into itself",
            Self::DestinationExists => "Destination already exists",
            Self::DestinationDirectoryNotFound => "Destination directory not found",
            Self::DestinationNotDirectory => "Destination path is not a directory",
            Self::DirectoryOverwriteForbidden => "Directories cannot be overwritten",
            Self::MoveStateConflict => {
                "Source or destination conflicts with an active upload or pending delete"
            }
            Self::RenameStateConflict => {
                "Rename source or destination conflicts with an active upload or pending delete"
            }
            Self::TargetNotFound => "Target not found",
            Self::DeleteStateConflict => "Target conflicts with an active upload or pending delete",
            Self::PurgeBacklogFull => "Delete backlog is temporarily full",
            Self::PurgeStateUnavailable => "Delete state storage is temporarily unavailable",
            Self::DeleteTargetChanged => "Delete target changed before commit",
            Self::DeleteNotCommitted => {
                "Delete was not committed; refresh the target before retrying"
            }
            Self::OutcomeUncertain => {
                "Operation outcome is uncertain; inspect the target before trying again"
            }
        }
    }

    pub(super) const fn recovery(self) -> RecoveryAdvice {
        match self {
            Self::OutcomeUncertain => RecoveryAdvice::QueryJob,
            _ => RecoveryAdvice::None,
        }
    }
}

impl OperationGuard {
    /// Mark the exact point after which cancellation can leave an uncertain
    /// filesystem outcome. Before this transition, dropping the guard removes
    /// the reservation and lets the same idempotency key retry safely.
    pub(super) async fn mark_commit_started(&mut self) -> anyhow::Result<()> {
        if self.completed {
            return Ok(());
        }
        if !self
            .registry
            .store
            .mark_operation_commit_started(self.key, self.lease)
            .await?
        {
            anyhow::bail!("operation reservation was lost before commit");
        }
        Ok(())
    }

    pub(super) async fn complete(mut self, outcome: OperationOutcome) -> anyhow::Result<()> {
        if !self
            .registry
            .store
            .complete_operation(self.key, self.lease, outcome.into_stored())
            .await?
        {
            anyhow::bail!("operation reservation was lost before completion");
        }
        self.completed = true;
        Ok(())
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.registry.store.abandon_operation(self.key, self.lease);
    }
}

pub(super) fn parse_operation_id(
    headers: &HeaderMap<HeaderValue>,
) -> Result<Option<Uuid>, &'static str> {
    let mut values = headers.get_all(OPERATION_ID_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("The x-dufs-operation-id header must appear exactly once");
    }
    let value = value
        .to_str()
        .map_err(|_| "The x-dufs-operation-id header must be a canonical UUID")?;
    parse_canonical_operation_id(value)
        .map(Some)
        .ok_or("The x-dufs-operation-id header must be a canonical UUID")
}

pub(super) fn parse_canonical_operation_id(value: &str) -> Option<Uuid> {
    let id = Uuid::parse_str(value).ok()?;
    (id.hyphenated().to_string() == value).then_some(id)
}

pub(super) fn apply_operation_outcome(
    res: &mut Response,
    id: Uuid,
    outcome: OperationOutcome,
    replayed: bool,
) -> anyhow::Result<()> {
    set_operation_headers(res, id, outcome.public_state());
    if replayed {
        res.headers_mut().insert(
            "x-dufs-operation-replayed",
            HeaderValue::from_static("true"),
        );
    }
    if let Some(error) = outcome.error {
        render_problem(
            res,
            &ApiError::new(outcome.status, error.code(), error.detail())
                .with_recovery(error.recovery())
                .with_operation(OperationProblemContext::new(
                    id.hyphenated().to_string(),
                    outcome.public_state(),
                    Some(outcome.status.as_u16()),
                )),
        )
    } else {
        *res.status_mut() = outcome.status;
        Ok(())
    }
}

pub(super) fn apply_running(res: &mut Response, id: Uuid) -> anyhow::Result<()> {
    *res.status_mut() = StatusCode::ACCEPTED;
    set_operation_headers(res, id, OperationPublicState::Running);
    res.headers_mut()
        .insert("retry-after", HeaderValue::from_static("1"));
    set_json_body(
        res,
        &OperationBody {
            operation_id: id.hyphenated().to_string(),
            state: OperationPublicState::Running,
            http_status: None,
            code: Some(ErrorCode::OPERATION_IN_PROGRESS),
            detail: Some("Operation is still running"),
        },
    )
}

pub(super) fn apply_conflict(res: &mut Response, id: Uuid) -> anyhow::Result<()> {
    set_operation_headers(res, id, OperationPublicState::Rejected);
    render_problem(
        res,
        &ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::OPERATION_ID_CONFLICT,
            "Operation ID was already used for a different request",
        )
        .with_operation(OperationProblemContext::new(
            id.hyphenated().to_string(),
            OperationPublicState::Rejected,
            None,
        )),
    )
}

pub(super) fn apply_registry_full(res: &mut Response, id: Uuid) -> anyhow::Result<()> {
    // Capacity is checked before inserting or executing the request, so this
    // outcome is a known rejection rather than an uncertain mutation.
    set_operation_headers(res, id, OperationPublicState::Rejected);
    render_problem(
        res,
        &ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::OPERATION_REGISTRY_FULL,
            "Operation registry is temporarily full",
        )
        .with_recovery(RecoveryAdvice::RetryAfterSeconds(1))
        .with_operation(OperationProblemContext::new(
            id.hyphenated().to_string(),
            OperationPublicState::Rejected,
            None,
        )),
    )
}

pub(super) fn apply_registry_unavailable(res: &mut Response, id: Uuid) -> anyhow::Result<()> {
    // Dispatch was rejected before the state actor accepted the command, so
    // the mutation definitely did not start and can be retried with the same
    // operation ID.
    set_operation_headers(res, id, OperationPublicState::Rejected);
    render_problem(
        res,
        &ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::OPERATION_STORE_UNAVAILABLE,
            "Operation state store is temporarily unavailable",
        )
        .with_recovery(RecoveryAdvice::RetryAfterSeconds(1))
        .with_operation(OperationProblemContext::new(
            id.hyphenated().to_string(),
            OperationPublicState::Rejected,
            None,
        )),
    )
}

pub(super) fn apply_status(
    res: &mut Response,
    id: Uuid,
    status: OperationStatus,
) -> anyhow::Result<()> {
    match status {
        OperationStatus::Running => {
            *res.status_mut() = StatusCode::OK;
            set_operation_headers(res, id, OperationPublicState::Running);
            set_json_body(
                res,
                &JobBody {
                    job_id: id.hyphenated().to_string(),
                    state: OperationPublicState::Running,
                    http_status: None,
                    code: None,
                    detail: None,
                },
            )
        }
        OperationStatus::Completed(outcome) => {
            *res.status_mut() = StatusCode::OK;
            set_operation_headers(res, id, outcome.public_state());
            set_json_body(
                res,
                &JobBody {
                    job_id: id.hyphenated().to_string(),
                    state: outcome.public_state(),
                    http_status: Some(outcome.status.as_u16()),
                    code: outcome.error.map(TrackedOperationError::code),
                    detail: outcome.error.map(TrackedOperationError::detail),
                },
            )
        }
        OperationStatus::NotFound => {
            set_operation_headers(res, id, OperationPublicState::Unknown);
            render_problem(
                res,
                &ApiError::new(
                    StatusCode::NOT_FOUND,
                    ErrorCode::JOB_NOT_FOUND,
                    "Job not found",
                )
                .with_recovery(RecoveryAdvice::RefreshTarget)
                .with_operation(OperationProblemContext::new(
                    id.hyphenated().to_string(),
                    OperationPublicState::Unknown,
                    None,
                )),
            )
        }
        OperationStatus::Unavailable => {
            set_operation_headers(res, id, OperationPublicState::Unknown);
            render_problem(
                res,
                &ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::JOB_STORE_UNAVAILABLE,
                    "Job state store is temporarily unavailable",
                )
                .with_recovery(RecoveryAdvice::RetryAfterSeconds(1))
                .with_operation(OperationProblemContext::new(
                    id.hyphenated().to_string(),
                    OperationPublicState::Unknown,
                    None,
                )),
            )
        }
    }
}

pub(super) fn apply_invalid_id(res: &mut Response, message: &'static str) -> anyhow::Result<()> {
    render_problem(
        res,
        &ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::INVALID_OPERATION_ID,
            message,
        ),
    )
}

pub(super) fn apply_invalid_job_id(res: &mut Response, detail: &'static str) -> anyhow::Result<()> {
    render_problem(
        res,
        &ApiError::new(StatusCode::BAD_REQUEST, ErrorCode::INVALID_JOB_ID, detail),
    )
}

pub(super) fn apply_unknown(
    res: &mut Response,
    id: Uuid,
    status: StatusCode,
    code: ErrorCode,
    message: &'static str,
) -> anyhow::Result<()> {
    set_operation_headers(res, id, OperationPublicState::Unknown);
    render_problem(
        res,
        &ApiError::new(status, code, message)
            .with_recovery(RecoveryAdvice::QueryJob)
            .with_operation(OperationProblemContext::new(
                id.hyphenated().to_string(),
                OperationPublicState::Unknown,
                None,
            )),
    )
}

pub(super) fn set_operation_headers(res: &mut Response, id: Uuid, state: OperationPublicState) {
    res.headers_mut().insert(
        OPERATION_ID_HEADER,
        HeaderValue::from_str(&id.hyphenated().to_string())
            .expect("a canonical UUID is a valid header value"),
    );
    res.headers_mut().insert(
        OPERATION_STATE_HEADER,
        HeaderValue::from_static(state.wire_name()),
    );
}

fn set_json_body<T: Serialize>(res: &mut Response, body: &T) -> anyhow::Result<()> {
    res.headers_mut()
        .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
    *res.body_mut() = body_full(serde_json::to_vec(body)?);
    Ok(())
}

#[cfg(test)]
mod tests;
