use super::{
    Request, Response, Server,
    disk_space::DiskSpaceReservation,
    identity::OwnerId,
    internal_names::upload_temp_path,
    maintenance::claim_changes as maintenance_claim_changes,
    path_coordinator::PathLease,
    problem::{ApiError, ErrorCode, RecoveryAdvice, UploadProblemContext, render_problem},
    protocol::UploadPublicState,
    rooted_fs::{
        CreatedAncestors, PreservedFileMetadata, ReplacementTargetIdentity, RootedEntryKey,
        RootedFs,
    },
    state_store::StateStoreDispatchError,
    status_not_found,
    storage::{CommitStagedFileOutcome, commit_staged_file, sync_file_to_storage},
};
use crate::http_utils::IncomingStream;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use futures_util::{Stream, TryStreamExt, pin_mut};
use headers::{ContentLength, HeaderMapExt};
use hyper::{StatusCode, header::HeaderValue};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::HashSet,
    fmt,
    io::SeekFrom,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    fs,
    io::{self, AsyncSeekExt, AsyncWriteExt},
    sync::watch,
    time::{Instant, timeout_at},
};
use uuid::Uuid;

mod commit;
mod failure;
mod prepare;
mod protocol;
mod record;
mod target;
mod transfer;

use protocol::{
    RESUMABLE_UPLOAD_MIN_SIZE, TARGET_REPLACEABLE_HEADER, TARGET_REVISION_HEADER, UPLOAD_ID_HEADER,
    UPLOAD_OFFSET_HEADER,
};
pub(super) use protocol::{
    TargetRevision, UploadMode, UploadOptions, UploadOverwritePolicy, apply_upload_record_headers,
    parse_upload_id, parse_upload_length, parse_upload_offset, parse_upload_overwrite,
};
pub(super) use record::UploadRecordStore;
use record::{
    UploadCheckpoint, UploadRecordContext, UploadRecordLookup, UploadRecordState,
    UploadRecordStoreError, rollback_upload_ancestors,
};
use target::apply_target_inspection_headers;

#[cfg(test)]
use super::internal_names::{
    DELETE_TRASH_PREFIX, DELETE_TRASH_SUFFIX, UPLOAD_TEMP_PREFIX, UPLOAD_TEMP_SUFFIX,
};
#[cfg(test)]
use super::maintenance::{
    MaintenanceBatchOptions, MaintenanceBudget, MaintenanceScanState, UPLOAD_SESSION_TTL,
    collect_and_remove_stale_internal_files, collect_stale_internal_files_batch,
};
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
#[cfg(test)]
use {
    super::rooted_fs::{DirectoryCursor, TrashEntry},
    crate::utils::get_file_name,
    std::time::SystemTime,
    tokio::io::AsyncReadExt,
};

const DISK_SPACE_RECHECK_BYTES: u64 = 8 * 1024 * 1024;
// RootedFs accepts at most 1 MiB of replacement xattrs. Add another 64 KiB
// for the checkpoint/state-temp inodes, directory entries, and filesystem
// metadata growth; disk_space rounds both data and this allowance to the
// actual f_frsize.
const UPLOAD_RESERVATION_METADATA_BYTES: u64 = 1024 * 1024 + 64 * 1024;

struct ActiveUploadFilesLease {
    active: Arc<Mutex<HashSet<RootedEntryKey>>>,
    keys: Vec<RootedEntryKey>,
}

/// Owns every resource that must stay alive from staging through publication.
///
/// Keeping the path/maintenance leases, disk reservation, staging descriptor,
/// and rollback state in one object makes the upload's cancellable and
/// non-cancellable boundaries explicit.
struct UploadTransaction<'a> {
    target_path: &'a Path,
    upload_path: PathBuf,
    owner_id: OwnerId,
    mode: UploadMode,
    upload_id: Uuid,
    upload_length: u64,
    deadline: Instant,
    target_identity: ReplacementTargetIdentity,
    target_revision: Option<[u8; 32]>,
    target_metadata: Option<PreservedFileMetadata>,
    created_ancestors: Option<CreatedAncestors>,
    file: Option<fs::File>,
    space_lease: DiskSpaceReservation,
    success_status: StatusCode,
    _path_lease: PathLease,
    _active_upload_files: ActiveUploadFilesLease,
}

/// Upload state after the request body has been consumed successfully but
/// before the final flush, length check, metadata replay, and checkpoint.
struct TransferredUpload<'a> {
    target_path: &'a Path,
    upload_path: PathBuf,
    owner_id: OwnerId,
    mode: UploadMode,
    upload_id: Uuid,
    upload_length: u64,
    deadline: Instant,
    target_identity: ReplacementTargetIdentity,
    target_revision: Option<[u8; 32]>,
    target_metadata: Option<PreservedFileMetadata>,
    created_ancestors: Option<CreatedAncestors>,
    file: fs::File,
    space_lease: DiskSpaceReservation,
    success_status: StatusCode,
    _path_lease: PathLease,
    _active_upload_files: ActiveUploadFilesLease,
}

/// State that has crossed the last cancellable pre-commit boundary and is
/// ready for metadata-preserving, durable publication.
struct ReadyUpload<'a> {
    target_path: &'a Path,
    upload_path: PathBuf,
    owner_id: OwnerId,
    upload_id: Uuid,
    upload_length: u64,
    target_identity: ReplacementTargetIdentity,
    target_revision: Option<[u8; 32]>,
    created_ancestors: Option<CreatedAncestors>,
    file: fs::File,
    success_status: StatusCode,
    _space_lease: DiskSpaceReservation,
    _path_lease: PathLease,
    _active_upload_files: ActiveUploadFilesLease,
}

#[derive(Clone, Copy)]
pub(in crate::server) struct UploadErrorContext {
    upload_id: Uuid,
    state: UploadPublicState,
    upload_length: Option<u64>,
    upload_offset: Option<u64>,
}

impl UploadErrorContext {
    pub(in crate::server) const fn new(
        upload_id: Uuid,
        state: UploadPublicState,
        upload_length: Option<u64>,
        upload_offset: Option<u64>,
    ) -> Self {
        Self {
            upload_id,
            state,
            upload_length,
            upload_offset,
        }
    }
}

pub(in crate::server) fn apply_upload_problem(
    res: &mut Response,
    context: UploadErrorContext,
    status: StatusCode,
    code: ErrorCode,
    detail: impl Into<Cow<'static, str>>,
    recovery: RecoveryAdvice,
) -> Result<()> {
    let problem = ApiError::new(status, code, detail)
        .with_recovery(recovery)
        .with_upload(UploadProblemContext::new(
            context.upload_id.to_string(),
            context.state,
            context.upload_length,
            context.upload_offset,
        ));
    render_problem(res, &problem)?;
    apply_upload_record_headers(
        res,
        context.upload_id,
        context.upload_length,
        context.upload_offset,
        context.state,
    )
}

/// Converts known upload-session storage failures into stable protocol errors.
/// A dispatch rejection happened before the SQLite actor accepted the command,
/// but an earlier checkpoint may still exist, so callers conservatively query
/// the upload ID instead of replaying the body directly.
fn apply_upload_record_store_problem(
    res: &mut Response,
    context: UploadErrorContext,
    error: &anyhow::Error,
) -> Result<bool> {
    if error.downcast_ref::<StateStoreDispatchError>().is_some() {
        apply_upload_problem(
            res,
            context,
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::UPLOAD_STATE_UNAVAILABLE,
            "Upload state storage is temporarily unavailable",
            RecoveryAdvice::QueryUpload,
        )?;
        return Ok(true);
    }
    let Some(store_error) = error.downcast_ref::<UploadRecordStoreError>() else {
        return Ok(false);
    };
    let (status, code, detail, recovery) = match store_error {
        UploadRecordStoreError::Conflict => (
            StatusCode::CONFLICT,
            ErrorCode::UPLOAD_SESSION_CONFLICT,
            "Upload ID conflicts with an existing session; query it before retrying",
            RecoveryAdvice::QueryUpload,
        ),
        UploadRecordStoreError::Full => (
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::UPLOAD_SESSION_STORE_FULL,
            "Upload session capacity is temporarily exhausted",
            RecoveryAdvice::RetryAfterSeconds(1),
        ),
    };
    apply_upload_problem(res, context, status, code, detail, recovery)?;
    Ok(true)
}
impl Drop for ActiveUploadFilesLease {
    fn drop(&mut self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in &self.keys {
            active.remove(key);
        }
    }
}

impl Server {
    pub(super) async fn handle_upload_status(
        &self,
        path: &Path,
        owner: &str,
        upload_id: Uuid,
        res: &mut Response,
    ) -> Result<()> {
        let upload_path = upload_temp_path(path, upload_id)?;
        let owner_id = OwnerId::persistent(owner);
        let lookup = match self
            .state
            .upload_records
            .lookup(owner_id, upload_id, path, &upload_path)
            .await
        {
            Ok(lookup) => lookup,
            Err(error) => {
                if apply_upload_record_store_problem(
                    res,
                    UploadErrorContext::new(upload_id, UploadPublicState::Unknown, None, None),
                    &error,
                )? {
                    return Ok(());
                }
                return Err(error);
            }
        };
        let checkpoint = match lookup {
            UploadRecordLookup::Found(checkpoint) => checkpoint,
            UploadRecordLookup::NotSeen | UploadRecordLookup::ForeignOwner => {
                status_not_found(res);
                apply_upload_record_headers(
                    res,
                    upload_id,
                    None,
                    None,
                    UploadPublicState::NotSeen,
                )?;
                return Ok(());
            }
        };
        let state = match checkpoint.state {
            UploadRecordState::Running => UploadPublicState::Running,
            UploadRecordState::AwaitingConfirmation => {
                *res.status_mut() = StatusCode::CONFLICT;
                let inspection = self.inspect_upload_target(owner, path).await?;
                apply_target_inspection_headers(res, inspection)?;
                UploadPublicState::AwaitingConfirmation
            }
            UploadRecordState::Committed => UploadPublicState::Committed,
            UploadRecordState::Rejected => {
                *res.status_mut() = StatusCode::CONFLICT;
                UploadPublicState::Rejected
            }
            UploadRecordState::Unknown => {
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::Unknown,
                        Some(checkpoint.upload_length),
                        Some(checkpoint.durable_offset),
                    ),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::UPLOAD_PUBLICATION_OUTCOME_UNKNOWN,
                    "Upload publication outcome is unknown; refresh the target before any retry",
                    RecoveryAdvice::RefreshTarget,
                )?;
                return Ok(());
            }
        };
        res.headers_mut()
            .typed_insert(ContentLength(checkpoint.durable_offset));
        apply_upload_record_headers(
            res,
            upload_id,
            Some(checkpoint.upload_length),
            Some(checkpoint.durable_offset),
            state,
        )?;
        Ok(())
    }

    pub(super) async fn discard_awaiting_upload(
        &self,
        owner: &str,
        path: &Path,
        upload_id: Uuid,
        res: &mut Response,
    ) -> Result<()> {
        let _path_lease = self.content.path_coordinator.acquire([path]).await;
        let owner_id = OwnerId::persistent(owner);
        let upload_path = upload_temp_path(path, upload_id)?;
        let record = match self
            .state
            .upload_records
            .lookup(owner_id, upload_id, path, &upload_path)
            .await?
        {
            UploadRecordLookup::Found(record) => record,
            UploadRecordLookup::NotSeen | UploadRecordLookup::ForeignOwner => {
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(upload_id, UploadPublicState::NotSeen, None, None),
                    StatusCode::NOT_FOUND,
                    ErrorCode::UPLOAD_SESSION_NOT_FOUND,
                    "Upload session not found",
                    RecoveryAdvice::None,
                )?;
                return Ok(());
            }
        };
        if record.state == UploadRecordState::Rejected {
            *res.status_mut() = StatusCode::NO_CONTENT;
            apply_upload_record_headers(
                res,
                upload_id,
                Some(record.upload_length),
                Some(record.durable_offset),
                UploadPublicState::Rejected,
            )?;
            return Ok(());
        }
        if record.state != UploadRecordState::AwaitingConfirmation {
            let state = match record.state {
                UploadRecordState::Running => UploadPublicState::Running,
                UploadRecordState::Committed => UploadPublicState::Committed,
                UploadRecordState::Unknown => UploadPublicState::Unknown,
                UploadRecordState::Rejected | UploadRecordState::AwaitingConfirmation => {
                    unreachable!()
                }
            };
            apply_upload_problem(
                res,
                UploadErrorContext::new(
                    upload_id,
                    state,
                    Some(record.upload_length),
                    Some(record.durable_offset),
                ),
                StatusCode::CONFLICT,
                ErrorCode::UPLOAD_STATE_CONFLICT,
                "Only an upload awaiting overwrite confirmation can be discarded",
                RecoveryAdvice::QueryUpload,
            )?;
            return Ok(());
        }

        self.state
            .upload_records
            .reset(owner_id, upload_id, path, &upload_path)
            .await?;
        self.state
            .upload_records
            .persist_terminal(
                UploadRecordContext::new(
                    owner_id,
                    upload_id,
                    path,
                    &upload_path,
                    record.upload_length,
                ),
                record.upload_length,
                UploadRecordState::Rejected,
            )
            .await?;
        *res.status_mut() = StatusCode::NO_CONTENT;
        apply_upload_record_headers(
            res,
            upload_id,
            Some(record.upload_length),
            Some(record.upload_length),
            UploadPublicState::Rejected,
        )?;
        Ok(())
    }

    pub(super) async fn handle_upload(
        &self,
        path: &Path,
        options: UploadOptions,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        let upload_id = options.upload_id;
        let upload_length = options.upload_length;
        let upload_offset = options.mode.offset();
        let result = async {
            let request_body_length = req
                .headers()
                .typed_get::<ContentLength>()
                .map(|value| value.0);
            let Some(upload) = self
                .prepare_upload(path, options, request_body_length, res)
                .await?
            else {
                return Ok(());
            };
            self.transfer_upload_body(upload, req, res).await
        }
        .await;
        if let Err(error) = result {
            if apply_upload_record_store_problem(
                res,
                UploadErrorContext::new(
                    upload_id,
                    UploadPublicState::Unknown,
                    Some(upload_length),
                    upload_offset,
                ),
                &error,
            )? {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
use failure::{UploadTimeout, finish_timed_out_upload, wait_for_maintenance_claim_change};
#[cfg(test)]
use prepare::{awaiting_stage_is_create_only, create_upload_temp};
#[cfg(test)]
use transfer::{UploadTransferError, UploadTransferOptions, receive_upload_body};

#[cfg(test)]
mod tests;
