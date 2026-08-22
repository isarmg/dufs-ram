use super::{
    failure::{
        UploadTimeout, apply_upload_unknown, finish_precommit_io_failure,
        finish_precommit_space_failure, finish_timed_out_upload,
    },
    *,
};

impl Server {
    pub(super) async fn prepare_transferred_upload(
        &self,
        upload: TransferredUpload<'_>,
        res: &mut Response,
    ) -> Result<()> {
        let TransferredUpload {
            target_path: path,
            upload_path,
            owner_id,
            mode,
            upload_id,
            upload_length,
            deadline,
            target_identity,
            target_revision,
            target_metadata,
            mut created_ancestors,
            mut file,
            mut space_lease,
            success_status: status,
            _path_lease: path_lease,
            _active_upload_files: active_upload_files,
        } = upload;
        let resume = mode.is_resume();
        let initial_offset = mode.offset().unwrap_or_default();

        // Tokio may still have a blocking write queued after `write_all`
        // resolves. Wait for that queue without cancelling the file future:
        // the outer tracked-upload deadline reports `unknown` to the client
        // while this task retains all leases until the queue is drained.
        if let Err(error) = file.flush().await {
            finish_precommit_io_failure(
                &self.state.upload_records,
                file,
                path,
                &upload_path,
                owner_id,
                upload_length,
                upload_id,
                initial_offset,
                resume,
                created_ancestors.take(),
                res,
                "Upload staging flush failed before commit",
                error,
            )
            .await?;
            return Ok(());
        }
        match space_lease
            .reserved_space_is_available_async(&file, self.content.args.min_free_space)
            .await
        {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                finish_precommit_space_failure(
                    &self.state.upload_records,
                    file,
                    path,
                    &upload_path,
                    owner_id,
                    upload_length,
                    upload_id,
                    initial_offset,
                    resume,
                    created_ancestors.take(),
                    res,
                    "Protected free disk space could not be confirmed before commit",
                )
                .await?;
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            finish_timed_out_upload(
                &self.state.upload_records,
                file,
                path,
                &upload_path,
                owner_id,
                upload_length,
                upload_id,
                res,
                UploadTimeout {
                    message: "Upload timed out before durable commit",
                    created_ancestors: created_ancestors.take(),
                },
            )
            .await?;
            return Ok(());
        }
        let actual_length = file.metadata().await?.len();
        if Instant::now() >= deadline {
            finish_timed_out_upload(
                &self.state.upload_records,
                file,
                path,
                &upload_path,
                owner_id,
                upload_length,
                upload_id,
                res,
                UploadTimeout {
                    message: "Upload timed out before durable commit",
                    created_ancestors: created_ancestors.take(),
                },
            )
            .await?;
            return Ok(());
        }
        if actual_length != upload_length {
            if actual_length < upload_length {
                self.state
                    .upload_records
                    .persist_checkpoint(
                        &mut file,
                        UploadRecordContext::new(
                            owner_id,
                            upload_id,
                            path,
                            &upload_path,
                            upload_length,
                        )
                        .with_target_revision(target_revision),
                        actual_length,
                    )
                    .await?;
                res.headers_mut().insert(
                    "x-dufs-upload-offset",
                    HeaderValue::from_str(&actual_length.to_string())?,
                );
            } else {
                drop(file);
                self.state
                    .upload_records
                    .reset_and_ancestors(
                        owner_id,
                        upload_id,
                        path,
                        &upload_path,
                        created_ancestors.take(),
                    )
                    .await?;
            }
            let resumable = actual_length < upload_length;
            let state = if resumable {
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
                    resumable.then_some(actual_length),
                ),
                StatusCode::CONFLICT,
                ErrorCode::UPLOAD_LENGTH_MISMATCH,
                format!(
                    "Upload is incomplete: expected {upload_length} bytes, stored {actual_length}"
                ),
                if resumable {
                    RecoveryAdvice::ResumeUpload
                } else {
                    RecoveryAdvice::RetryWithNewId
                },
            )?;
            return Ok(());
        }

        if Instant::now() >= deadline {
            finish_timed_out_upload(
                &self.state.upload_records,
                file,
                path,
                &upload_path,
                owner_id,
                upload_length,
                upload_id,
                res,
                UploadTimeout {
                    message: "Upload timed out before durable commit",
                    created_ancestors: created_ancestors.take(),
                },
            )
            .await?;
            return Ok(());
        }

        // The body has already been flushed and its length checked. This is
        // the final cancellable boundary: metadata replay can make the hidden
        // stage read-only (for example when replacing a 0444 target), so it
        // and the following typed sync/rename/parent-sync commit must form one
        // non-cancellable segment. Otherwise a deadline between chmod and
        // rename could advertise a checkpoint that PATCH cannot reopen.
        if let Some(metadata) = target_metadata {
            // Dropping this blocking operation's join future would not stop
            // fchown/fchmod or xattr mutation. Await it fully; the outer
            // deadline can report `unknown`, while this tracked task retains
            // every lease and proceeds directly to the durable commit.
            let metadata_result = self
                .content
                .rooted_fs
                .apply_replacement_metadata(&file, metadata)
                .await;
            if let Err(error) = metadata_result {
                drop(file);
                self.state
                    .upload_records
                    .reset_and_ancestors(
                        owner_id,
                        upload_id,
                        path,
                        &upload_path,
                        created_ancestors.take(),
                    )
                    .await?;
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Unsupported
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidData
                ) {
                    apply_upload_problem(
                        res,
                        UploadErrorContext::new(
                            upload_id,
                            UploadPublicState::Rejected,
                            Some(upload_length),
                            None,
                        ),
                        StatusCode::CONFLICT,
                        ErrorCode::UPLOAD_METADATA_PRESERVATION_REFUSED,
                        "Cannot preserve target metadata; overwrite was refused",
                        RecoveryAdvice::RefreshTarget,
                    )?;
                    return Ok(());
                }
                return Err(error.into());
            }
        }
        match space_lease
            .reserved_space_is_available_async(&file, self.content.args.min_free_space)
            .await
        {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                drop(file);
                let cleanup = self
                    .state
                    .upload_records
                    .reset_and_ancestors(
                        owner_id,
                        upload_id,
                        path,
                        &upload_path,
                        created_ancestors.take(),
                    )
                    .await;
                if let Err(error) = cleanup {
                    error!(
                        "Upload rollback failed after metadata replay target={} upload_id={} error={error:#}",
                        path.display(),
                        upload_id
                    );
                    apply_upload_unknown(
                        res,
                        upload_id,
                        "Upload rollback could not be confirmed after metadata replay",
                    )?;
                } else {
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
                        "Insufficient protected free disk space before publication",
                        RecoveryAdvice::RetryWithNewId,
                    )?;
                }
                return Ok(());
            }
        }
        if let Err(error) = self
            .state
            .upload_records
            .persist_commit_started(
                &mut file,
                UploadRecordContext::new(owner_id, upload_id, path, &upload_path, upload_length)
                    .with_target_revision(target_revision),
            )
            .await
        {
            error!(
                "Failed to persist the final running upload record target={} upload_id={} error={error:#}",
                path.display(),
                upload_id
            );
            drop(file);
            if let Err(cleanup_error) = self
                .state
                .upload_records
                .reset_and_ancestors(
                    owner_id,
                    upload_id,
                    path,
                    &upload_path,
                    created_ancestors.take(),
                )
                .await
            {
                error!(
                    "Upload rollback failed after final-record persistence target={} upload_id={} error={cleanup_error:#}",
                    path.display(),
                    upload_id
                );
                apply_upload_unknown(
                    res,
                    upload_id,
                    "Upload rollback could not be confirmed before publication",
                )?;
            } else {
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::Rejected,
                        Some(upload_length),
                        None,
                    ),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::UPLOAD_CHECKPOINT_PERSIST_FAILED,
                    "Upload was not published; retry with a new upload ID",
                    RecoveryAdvice::RetryWithNewId,
                )?;
            }
            return Ok(());
        }
        match space_lease
            .reserved_space_is_available_async(&file, self.content.args.min_free_space)
            .await
        {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                drop(file);
                if let Err(error) = self
                    .state
                    .upload_records
                    .reset_and_ancestors(
                        owner_id,
                        upload_id,
                        path,
                        &upload_path,
                        created_ancestors.take(),
                    )
                    .await
                {
                    error!(
                        "Upload rollback failed after final capacity verification target={} upload_id={} error={error:#}",
                        path.display(),
                        upload_id
                    );
                    apply_upload_unknown(
                        res,
                        upload_id,
                        "Upload rollback could not be confirmed before publication",
                    )?;
                } else {
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
                        "Insufficient protected free disk space before publication",
                        RecoveryAdvice::RetryWithNewId,
                    )?;
                }
                return Ok(());
            }
        }

        self.commit_ready_upload(
            ReadyUpload {
                target_path: path,
                upload_path,
                owner_id,
                upload_id,
                upload_length,
                target_identity,
                target_revision,
                created_ancestors,
                file,
                success_status: status,
                _space_lease: space_lease,
                _path_lease: path_lease,
                _active_upload_files: active_upload_files,
            },
            res,
        )
        .await
    }

    async fn commit_ready_upload(&self, upload: ReadyUpload<'_>, res: &mut Response) -> Result<()> {
        let ReadyUpload {
            target_path: path,
            upload_path,
            owner_id,
            upload_id,
            upload_length,
            target_identity,
            target_revision,
            mut created_ancestors,
            file,
            success_status: status,
            _space_lease,
            _path_lease: path_lease,
            _active_upload_files: active_upload_files,
        } = upload;

        // The complete upload handler runs as a tracked mutation task, so these
        // guards remain alive even if Hyper drops the request future after a
        // browser or gateway disconnect. The final rename and directory sync
        // therefore cannot outlive their path and maintenance leases.
        let _path_lease = path_lease;
        let _active_upload_files = active_upload_files;
        match commit_staged_file(
            &self.content.storage,
            file,
            &upload_path,
            path,
            target_identity,
        )
        .await
        {
            CommitStagedFileOutcome::Published => {}
            CommitStagedFileOutcome::Rejected(mut file) => {
                warn!(
                    "Upload commit rejected because a filesystem entry changed target={} upload_id={}",
                    path.display(),
                    upload_id,
                );
                // The checked replace returned before rename, so the upload
                // outcome is known and the same fully durable stage can be
                // retained for an explicit conditional publish.
                if let Err(error) = self
                    .state
                    .upload_records
                    .persist_awaiting_confirmation(
                        &mut file,
                        UploadRecordContext::new(
                            owner_id,
                            upload_id,
                            path,
                            &upload_path,
                            upload_length,
                        )
                        .with_target_revision(target_revision),
                    )
                    .await
                {
                    error!(
                        "Failed to persist awaiting-confirmation upload target={} upload_id={} error={error:#}",
                        path.display(),
                        upload_id
                    );
                    apply_upload_unknown(
                        res,
                        upload_id,
                        "Upload was not published, but its retained stage could not be confirmed",
                    )?;
                    return Ok(());
                }
                drop(file);
                // Ancestors now contain the durable retained stage and must
                // remain until publish, discard, or TTL maintenance cleanup.
                let _created_ancestors = created_ancestors.take();
                self.render_upload_target_conflict(
                    path,
                    owner_id,
                    UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::AwaitingConfirmation,
                        Some(upload_length),
                        Some(upload_length),
                    ),
                    res,
                )
                .await?;
                return Ok(());
            }
            CommitStagedFileOutcome::NotPublished(error) => {
                error!(
                    "Upload commit failed before publication target={} upload_id={} error={error:#}",
                    path.display(),
                    upload_id,
                );
                self.state
                    .upload_records
                    .reset_and_ancestors(
                        owner_id,
                        upload_id,
                        path,
                        &upload_path,
                        created_ancestors.take(),
                    )
                    .await?;
                if let Err(record_error) = self
                    .state
                    .upload_records
                    .persist_terminal(
                        UploadRecordContext::new(
                            owner_id,
                            upload_id,
                            path,
                            &upload_path,
                            upload_length,
                        ),
                        0,
                        UploadRecordState::Rejected,
                    )
                    .await
                {
                    warn!(
                        "Failed to persist pre-publication rejection target={} upload_id={} error={record_error:#}",
                        path.display(),
                        upload_id
                    );
                }
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::Rejected,
                        Some(upload_length),
                        None,
                    ),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::UPLOAD_NOT_PUBLISHED,
                    "Upload was not published; retry the upload",
                    RecoveryAdvice::RetryWithNewId,
                )?;
                return Ok(());
            }
            CommitStagedFileOutcome::PublishedDurabilityUnknown(error) => {
                error!(
                    "Upload commit outcome is unknown target={} upload_id={} error={error:#}",
                    path.display(),
                    upload_id,
                );
                if let Err(record_error) = self
                    .state
                    .upload_records
                    .persist_unknown(
                        UploadRecordContext::new(
                            owner_id,
                            upload_id,
                            path,
                            &upload_path,
                            upload_length,
                        ),
                        upload_length,
                    )
                    .await
                {
                    error!(
                        "Failed to persist unknown upload publication target={} upload_id={} error={record_error:#}",
                        path.display(),
                        upload_id,
                    );
                }
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::Unknown,
                        Some(upload_length),
                        Some(upload_length),
                    ),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::UPLOAD_PUBLICATION_DURABILITY_UNKNOWN,
                    "Upload commit was not confirmed; refresh the folder before trying again",
                    RecoveryAdvice::QueryUpload,
                )?;
                return Ok(());
            }
        }

        if let Err(error) = self
            .state
            .upload_records
            .persist_terminal(
                UploadRecordContext::new(owner_id, upload_id, path, &upload_path, upload_length),
                upload_length,
                UploadRecordState::Committed,
            )
            .await
        {
            error!(
                "Upload committed but its terminal record was not durable target={} upload_id={} error={error:#}",
                path.display(),
                upload_id,
            );
            apply_upload_problem(
                res,
                UploadErrorContext::new(
                    upload_id,
                    UploadPublicState::Unknown,
                    Some(upload_length),
                    Some(upload_length),
                ),
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::UPLOAD_TERMINAL_RECORD_FAILED,
                "Upload was published, but terminal confirmation could not be persisted; query the upload ID before any retry",
                RecoveryAdvice::QueryUpload,
            )?;
            return Ok(());
        }

        *res.status_mut() = status;
        apply_upload_record_headers(
            res,
            upload_id,
            Some(upload_length),
            Some(upload_length),
            UploadPublicState::Committed,
        )?;
        Ok(())
    }
}
