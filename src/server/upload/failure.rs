use super::*;

pub(super) fn apply_upload_unknown(
    res: &mut Response,
    upload_id: Uuid,
    message: &'static str,
) -> Result<()> {
    apply_upload_problem(
        res,
        UploadErrorContext::new(upload_id, UploadPublicState::Unknown, None, None),
        StatusCode::REQUEST_TIMEOUT,
        ErrorCode::UPLOAD_OUTCOME_UNKNOWN,
        message,
        RecoveryAdvice::QueryUpload,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_precommit_space_failure(
    upload_records: &UploadRecordStore,
    file: fs::File,
    target_path: &Path,
    upload_path: &Path,
    owner_id: OwnerId,
    upload_length: u64,
    upload_id: Uuid,
    initial_offset: u64,
    resume: bool,
    created_ancestors: Option<CreatedAncestors>,
    res: &mut Response,
    message: &'static str,
) -> Result<()> {
    if resume {
        if let Err(error) = file.set_len(initial_offset).await {
            warn!(
                "Failed to truncate an uncommitted upload after a capacity check upload_id={} error={error:#}",
                upload_id
            );
        } else if let Err(error) = sync_file_to_storage(&file).await {
            warn!(
                "Failed to sync a truncated upload after a capacity check upload_id={} error={error:#}",
                upload_id
            );
        }
        drop(file);
        apply_upload_problem(
            res,
            UploadErrorContext::new(
                upload_id,
                UploadPublicState::Running,
                Some(upload_length),
                Some(initial_offset),
            ),
            StatusCode::INSUFFICIENT_STORAGE,
            ErrorCode::UPLOAD_INSUFFICIENT_STORAGE,
            message,
            RecoveryAdvice::ResumeUpload,
        )?;
        return Ok(());
    }

    drop(file);
    if let Err(error) = upload_records
        .reset_and_ancestors(
            owner_id,
            upload_id,
            target_path,
            upload_path,
            created_ancestors,
        )
        .await
    {
        error!(
            "Failed to roll back a fresh upload after a capacity check upload_id={} error={error:#}",
            upload_id
        );
        apply_upload_unknown(
            res,
            upload_id,
            "Upload rollback could not be confirmed after a disk-space failure",
        )?;
        return Ok(());
    }
    apply_upload_problem(
        res,
        UploadErrorContext::new(
            upload_id,
            UploadPublicState::Rejected,
            Some(upload_length),
            None,
        ),
        StatusCode::INSUFFICIENT_STORAGE,
        ErrorCode::UPLOAD_INSUFFICIENT_STORAGE,
        message,
        RecoveryAdvice::RetryWithNewId,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_precommit_io_failure(
    upload_records: &UploadRecordStore,
    file: fs::File,
    target_path: &Path,
    upload_path: &Path,
    owner_id: OwnerId,
    upload_length: u64,
    upload_id: Uuid,
    initial_offset: u64,
    resume: bool,
    created_ancestors: Option<CreatedAncestors>,
    res: &mut Response,
    message: &'static str,
    source: io::Error,
) -> Result<()> {
    error!(
        "Upload failed before publication upload_id={} phase=precommit error={source:#}",
        upload_id
    );
    drop(file);
    if !resume
        && let Err(error) = upload_records
            .reset_and_ancestors(
                owner_id,
                upload_id,
                target_path,
                upload_path,
                created_ancestors,
            )
            .await
    {
        error!(
            "Failed to roll back a fresh precommit upload upload_id={} error={error:#}",
            upload_id
        );
        apply_upload_unknown(
            res,
            upload_id,
            "Upload rollback could not be confirmed after a precommit failure",
        )?;
        return Ok(());
    }
    let state = if resume {
        UploadPublicState::Running
    } else {
        UploadPublicState::Rejected
    };
    apply_upload_problem(
        res,
        UploadErrorContext::new(
            upload_id,
            state,
            Some(upload_length),
            resume.then_some(initial_offset),
        ),
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::UPLOAD_PRECOMMIT_FAILED,
        message,
        if resume {
            RecoveryAdvice::ResumeUpload
        } else {
            RecoveryAdvice::RetryWithNewId
        },
    )
}

pub(super) struct UploadTimeout {
    pub(super) message: &'static str,
    pub(super) created_ancestors: Option<CreatedAncestors>,
}

// The timeout finalizer deliberately receives the live file, its rooted
// session identity and the response separately: bundling those borrowed and
// owned values would obscure which resources remain held across the final
// flush/checkpoint boundary.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_timed_out_upload(
    upload_records: &UploadRecordStore,
    mut file: fs::File,
    target_path: &Path,
    upload_path: &Path,
    owner_id: OwnerId,
    upload_length: u64,
    upload_id: Uuid,
    res: &mut Response,
    timeout: UploadTimeout,
) -> Result<()> {
    let UploadTimeout {
        message,
        created_ancestors,
    } = timeout;
    let fresh = created_ancestors.is_some();
    // A Tokio file may still be draining a queued blocking write. Keep the
    // caller's path and active-session leases alive until it is complete,
    // then apply the same checkpoint threshold as idle and I/O failures.
    if let Err(error) = file.flush().await {
        let discard_result = if fresh {
            Some(
                upload_records
                    .discard_unrecorded_stage(&file, upload_path)
                    .await,
            )
        } else {
            None
        };
        drop(file);
        if created_ancestors.is_some() {
            upload_records
                .reset_and_ancestors(
                    owner_id,
                    upload_id,
                    target_path,
                    upload_path,
                    created_ancestors,
                )
                .await?;
        }
        if let Some(discard_result) = discard_result {
            discard_result?;
        }
        return Err(error.into());
    }
    let partial_size = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            drop(file);
            if created_ancestors.is_some() {
                upload_records
                    .reset_and_ancestors(
                        owner_id,
                        upload_id,
                        target_path,
                        upload_path,
                        created_ancestors,
                    )
                    .await?;
            }
            return Err(error.into());
        }
    };
    if partial_size >= RESUMABLE_UPLOAD_MIN_SIZE && partial_size <= upload_length {
        let checkpoint_result = if fresh {
            upload_records
                .persist_initial_checkpoint(
                    &mut file,
                    UploadRecordContext::new(
                        owner_id,
                        upload_id,
                        target_path,
                        upload_path,
                        upload_length,
                    ),
                    partial_size,
                )
                .await
        } else {
            upload_records
                .persist_checkpoint(
                    &mut file,
                    UploadRecordContext::new(
                        owner_id,
                        upload_id,
                        target_path,
                        upload_path,
                        upload_length,
                    ),
                    partial_size,
                )
                .await
        };
        if let Err(error) = checkpoint_result {
            let discard_result = if fresh {
                Some(
                    upload_records
                        .discard_unrecorded_stage(&file, upload_path)
                        .await,
                )
            } else {
                None
            };
            drop(file);
            if created_ancestors.is_some() {
                upload_records
                    .reset_and_ancestors(
                        owner_id,
                        upload_id,
                        target_path,
                        upload_path,
                        created_ancestors,
                    )
                    .await?;
            }
            if let Some(discard_result) = discard_result {
                discard_result?;
            }
            return Err(error);
        }
        res.headers_mut().insert(
            UPLOAD_OFFSET_HEADER,
            HeaderValue::from_str(&partial_size.to_string())?,
        );
    } else {
        // Below the resumability threshold the session is deliberately
        // discarded, whether this was a fresh PUT or a resumed PATCH. Use the
        // still-open descriptor as the deletion capability: the owner-scoped
        // database row may not exist yet, and a pathname/UUID alone must never
        // authorize removal of another owner's stage.
        let discard_result = upload_records
            .discard_unrecorded_stage(&file, upload_path)
            .await;
        drop(file);
        upload_records
            .reset_and_ancestors(
                owner_id,
                upload_id,
                target_path,
                upload_path,
                created_ancestors,
            )
            .await?;
        discard_result?;
    }
    apply_upload_unknown(res, upload_id, message)
}

pub(super) async fn wait_for_maintenance_claim_change(
    claim_changes: &mut watch::Receiver<u64>,
    deadline: Instant,
    force_shutdown: &tokio_util::sync::CancellationToken,
) -> io::Result<()> {
    tokio::select! {
        biased;
        _ = force_shutdown.cancelled() => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "server shutdown interrupted upload preparation",
        )),
        _ = tokio::time::sleep_until(deadline) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "upload deadline exceeded while waiting for maintenance cleanup",
        )),
        result = claim_changes.changed() => result.map_err(|_| {
            io::Error::other("upload maintenance claim notification channel closed")
        }),
    }
}

pub(super) fn upload_deadline_expired(
    deadline: Instant,
    res: &mut Response,
    upload_id: Uuid,
    message: &'static str,
) -> Result<bool> {
    if Instant::now() < deadline {
        return Ok(false);
    }
    apply_upload_unknown(res, upload_id, message)?;
    Ok(true)
}

pub(super) fn upload_deadline_expired_before_mutation(
    deadline: Instant,
    res: &mut Response,
    upload_id: Uuid,
    upload_length: u64,
    upload_offset: Option<u64>,
    message: &'static str,
) -> Result<bool> {
    if Instant::now() < deadline {
        return Ok(false);
    }
    apply_upload_problem(
        res,
        UploadErrorContext::new(
            upload_id,
            UploadPublicState::NotStarted,
            Some(upload_length),
            upload_offset,
        ),
        StatusCode::REQUEST_TIMEOUT,
        ErrorCode::REQUEST_TIMEOUT,
        message,
        RecoveryAdvice::Retry,
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    #[tokio::test]
    async fn pre_mutation_deadline_is_not_reported_as_unknown() {
        let upload_id = Uuid::new_v4();
        let mut response = Response::default();

        assert!(
            upload_deadline_expired_before_mutation(
                Instant::now() - Duration::from_millis(1),
                &mut response,
                upload_id,
                17,
                Some(4),
                "Upload timed out during a read-only check",
            )
            .unwrap()
        );

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(response.headers()["x-dufs-operation-state"], "not-started");
        assert_eq!(
            response.headers()["x-dufs-upload-id"],
            upload_id.to_string()
        );
        assert_eq!(response.headers()["x-dufs-upload-length"], "17");
        assert_eq!(response.headers()["x-dufs-upload-offset"], "4");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "request_timeout");
        assert_eq!(problem["recovery"], "retry");
        assert_eq!(problem["upload_state"], "not-started");
    }
}
