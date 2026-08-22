use super::{
    failure::{
        UploadTimeout, finish_precommit_space_failure, finish_timed_out_upload,
        upload_deadline_expired, wait_for_maintenance_claim_change,
    },
    target::{inspect_target_identity, target_revision},
    *,
};

pub(super) fn awaiting_stage_is_create_only(checkpoint: &UploadCheckpoint) -> bool {
    checkpoint.state == UploadRecordState::AwaitingConfirmation
        && checkpoint.target_revision.is_none()
}

impl Server {
    async fn track_active_upload_files(
        &self,
        upload_path: &Path,
        deadline: Instant,
    ) -> std::io::Result<ActiveUploadFilesLease> {
        let paths = [upload_path.to_path_buf()];
        loop {
            // Subscribe before inspecting markers so a cleanup completion
            // between the check and `changed()` cannot be lost.
            let mut claim_changes = maintenance_claim_changes().subscribe();
            let mut keys = Vec::with_capacity(paths.len());
            for path in &paths {
                keys.push(self.content.rooted_fs.entry_key(path).await?);
            }
            let cleanup_markers = keys
                .iter()
                .map(RootedEntryKey::maintenance_marker)
                .collect::<Vec<_>>();
            let cleanup_in_progress = {
                let mut active = self
                    .admission
                    .active_upload_files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if cleanup_markers.iter().any(|marker| active.contains(marker)) {
                    true
                } else {
                    for key in &keys {
                        active.insert(key.clone());
                    }
                    false
                }
            };
            if !cleanup_in_progress {
                return Ok(ActiveUploadFilesLease {
                    active: self.admission.active_upload_files.clone(),
                    keys,
                });
            }

            // Cleanup performs its filesystem operation without holding the
            // registry mutex. Wait on the process-wide claim generation so an
            // abnormal filesystem neither causes busy polling nor adds more
            // blocking path lookups. Recompute semantic keys after wakeup,
            // because an external actor may have replaced a parent directory.
            loop {
                let cleanup_still_in_progress = {
                    let active = self
                        .admission
                        .active_upload_files
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    cleanup_markers.iter().any(|marker| active.contains(marker))
                };
                if !cleanup_still_in_progress {
                    break;
                }
                wait_for_maintenance_claim_change(
                    &mut claim_changes,
                    deadline,
                    &self.lifecycle.force_shutdown,
                )
                .await?;
            }
        }
    }

    pub(super) async fn prepare_upload<'a>(
        &'a self,
        path: &'a Path,
        options: UploadOptions,
        request_body_length: Option<u64>,
        res: &mut Response,
    ) -> Result<Option<UploadTransaction<'a>>> {
        let UploadOptions {
            owner,
            mode,
            upload_id,
            upload_length,
            overwrite,
            deadline,
            path_lease,
        } = options;
        let resume = mode.is_resume();
        let upload_offset = mode.offset();
        let owner_id = OwnerId::persistent(&owner);
        let requested_revision = overwrite.revision().map(TargetRevision::into_bytes);
        res.headers_mut().insert(
            UPLOAD_ID_HEADER,
            HeaderValue::from_str(&upload_id.to_string())?,
        );
        if upload_deadline_expired(
            deadline,
            res,
            upload_id,
            "Upload deadline exceeded during preparation",
        )? {
            return Ok(None);
        }
        let upload_path = upload_temp_path(path, upload_id)?;
        let existing_record = match self
            .state
            .upload_records
            .lookup(owner_id, upload_id, path, &upload_path)
            .await?
        {
            UploadRecordLookup::Found(record) => Some(record),
            UploadRecordLookup::NotSeen => None,
            UploadRecordLookup::ForeignOwner => {
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(upload_id, UploadPublicState::NotSeen, None, None),
                    StatusCode::NOT_FOUND,
                    ErrorCode::UPLOAD_SESSION_NOT_FOUND,
                    "Not Found",
                    RecoveryAdvice::RetryWithNewId,
                )?;
                return Ok(None);
            }
        };
        if let Some(record) = existing_record.as_ref() {
            match record.state {
                UploadRecordState::Committed => {
                    let same_length = record.upload_length == upload_length;
                    *res.status_mut() = if same_length {
                        StatusCode::OK
                    } else {
                        StatusCode::CONFLICT
                    };
                    if !same_length {
                        apply_upload_problem(
                            res,
                            UploadErrorContext::new(
                                upload_id,
                                UploadPublicState::Committed,
                                Some(record.upload_length),
                                Some(record.durable_offset),
                            ),
                            StatusCode::CONFLICT,
                            ErrorCode::UPLOAD_COMMITTED_LENGTH_MISMATCH,
                            format!(
                                "Upload ID is already committed with length {}",
                                record.upload_length
                            ),
                            RecoveryAdvice::QueryUpload,
                        )?;
                        return Ok(None);
                    }
                    apply_upload_record_headers(
                        res,
                        upload_id,
                        Some(record.upload_length),
                        Some(record.durable_offset),
                        UploadPublicState::Committed,
                    )?;
                    return Ok(None);
                }
                UploadRecordState::Rejected => {
                    apply_upload_problem(
                        res,
                        UploadErrorContext::new(
                            upload_id,
                            UploadPublicState::Rejected,
                            Some(record.upload_length),
                            Some(record.durable_offset),
                        ),
                        StatusCode::CONFLICT,
                        ErrorCode::UPLOAD_ID_REJECTED,
                        "Upload ID was rejected; start with a new upload ID",
                        RecoveryAdvice::RetryWithNewId,
                    )?;
                    return Ok(None);
                }
                UploadRecordState::Running if !resume => {
                    apply_upload_problem(
                        res,
                        UploadErrorContext::new(
                            upload_id,
                            UploadPublicState::Running,
                            Some(record.upload_length),
                            Some(record.durable_offset),
                        ),
                        StatusCode::CONFLICT,
                        ErrorCode::UPLOAD_IN_PROGRESS,
                        "Upload ID is already running or awaiting terminal confirmation; query its status",
                        RecoveryAdvice::QueryUpload,
                    )?;
                    return Ok(None);
                }
                UploadRecordState::Running if record.durable_offset == record.upload_length => {
                    apply_upload_problem(
                        res,
                        UploadErrorContext::new(
                            upload_id,
                            UploadPublicState::Running,
                            Some(record.upload_length),
                            Some(record.durable_offset),
                        ),
                        StatusCode::CONFLICT,
                        ErrorCode::UPLOAD_IN_PROGRESS,
                        "Upload ID is awaiting terminal confirmation; query its status",
                        RecoveryAdvice::QueryUpload,
                    )?;
                    return Ok(None);
                }
                UploadRecordState::Running => {}
                UploadRecordState::AwaitingConfirmation
                    if !resume || upload_offset != Some(record.upload_length) =>
                {
                    self.render_upload_target_conflict(
                        path,
                        owner_id,
                        UploadErrorContext::new(
                            upload_id,
                            UploadPublicState::AwaitingConfirmation,
                            Some(record.upload_length),
                            Some(record.durable_offset),
                        ),
                        res,
                    )
                    .await?;
                    return Ok(None);
                }
                UploadRecordState::AwaitingConfirmation => {}
                UploadRecordState::Unknown => {
                    apply_upload_problem(
                        res,
                        UploadErrorContext::new(
                            upload_id,
                            UploadPublicState::Unknown,
                            Some(record.upload_length),
                            Some(record.durable_offset),
                        ),
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ErrorCode::UPLOAD_PUBLICATION_OUTCOME_UNKNOWN,
                        "Upload publication outcome is unknown; query or refresh before retrying",
                        RecoveryAdvice::QueryUpload,
                    )?;
                    return Ok(None);
                }
            }
        }
        if upload_length > self.content.args.max_upload_size {
            apply_upload_problem(
                res,
                UploadErrorContext::new(
                    upload_id,
                    UploadPublicState::Rejected,
                    Some(upload_length),
                    None,
                ),
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::UPLOAD_SIZE_LIMIT_EXCEEDED,
                format!(
                    "Upload length {upload_length} exceeds the configured maximum of {} bytes",
                    self.content.args.max_upload_size
                ),
                RecoveryAdvice::None,
            )?;
            return Ok(None);
        }
        let mut active_upload_files = if resume {
            Some(
                match self.track_active_upload_files(&upload_path, deadline).await {
                    Ok(lease) => lease,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        apply_upload_problem(
                            res,
                            UploadErrorContext::new(
                                upload_id,
                                UploadPublicState::NotSeen,
                                None,
                                None,
                            ),
                            StatusCode::NOT_FOUND,
                            ErrorCode::UPLOAD_SESSION_NOT_FOUND,
                            "Not Found",
                            RecoveryAdvice::RetryWithNewId,
                        )?;
                        return Ok(None);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                        upload_deadline_expired(
                            deadline,
                            res,
                            upload_id,
                            "Upload deadline exceeded during preparation",
                        )?;
                        return Ok(None);
                    }
                    Err(error) => return Err(error.into()),
                },
            )
        } else {
            None
        };
        if resume
            && upload_deadline_expired(
                deadline,
                res,
                upload_id,
                "Upload deadline exceeded during preparation",
            )?
        {
            return Ok(None);
        }
        let conflict_state = match existing_record.as_ref().map(|record| record.state) {
            Some(UploadRecordState::AwaitingConfirmation) => {
                UploadPublicState::AwaitingConfirmation
            }
            Some(UploadRecordState::Running) => UploadPublicState::Running,
            _ => UploadPublicState::NotStarted,
        };
        let conflict_offset = existing_record.as_ref().map(|record| record.durable_offset);
        let inspected_identity = self.content.rooted_fs.replacement_identity(path).await?;
        let canonical_relative_path = self.content.rooted_fs.state_relative_path(path)?;
        let inspection =
            inspect_target_identity(owner_id, &canonical_relative_path, inspected_identity);
        let running_no_replace_may_finish_staging = resume
            && existing_record
                .as_ref()
                .is_some_and(|record| record.state == UploadRecordState::Running);
        let (target_identity, target_metadata) = match overwrite {
            UploadOverwritePolicy::NoReplace
                if inspection.exists && !running_no_replace_may_finish_staging =>
            {
                apply_target_inspection_headers(res, inspection)?;
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(
                        upload_id,
                        conflict_state,
                        Some(upload_length),
                        conflict_offset,
                    ),
                    StatusCode::CONFLICT,
                    ErrorCode::DESTINATION_EXISTS,
                    "The upload destination exists and requires explicit overwrite confirmation",
                    RecoveryAdvice::RefreshTarget,
                )?;
                return Ok(None);
            }
            UploadOverwritePolicy::NoReplace
                if existing_record.as_ref().is_some_and(|record| {
                    record.state == UploadRecordState::AwaitingConfirmation
                        && !awaiting_stage_is_create_only(record)
                }) =>
            {
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::AwaitingConfirmation,
                        Some(upload_length),
                        conflict_offset,
                    ),
                    StatusCode::CONFLICT,
                    ErrorCode::UPLOAD_METADATA_PRESERVATION_REFUSED,
                    "The retained stage contains metadata from a replaced target; discard it and start a new create-only upload",
                    RecoveryAdvice::RefreshTarget,
                )?;
                return Ok(None);
            }
            UploadOverwritePolicy::NoReplace => (ReplacementTargetIdentity::Missing, None),
            UploadOverwritePolicy::IfUnchanged(expected)
                if inspection.revision != Some(expected) || !inspection.replaceable =>
            {
                self.render_upload_target_conflict(
                    path,
                    owner_id,
                    UploadErrorContext::new(
                        upload_id,
                        conflict_state,
                        Some(upload_length),
                        conflict_offset,
                    ),
                    res,
                )
                .await?;
                return Ok(None);
            }
            UploadOverwritePolicy::IfUnchanged(expected) => {
                let target = match self.content.rooted_fs.replacement_metadata(path).await {
                    Ok(target) => target,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::Unsupported
                                | std::io::ErrorKind::PermissionDenied
                                | std::io::ErrorKind::InvalidData
                        ) =>
                    {
                        self.render_upload_target_conflict(
                            path,
                            owner_id,
                            UploadErrorContext::new(
                                upload_id,
                                conflict_state,
                                Some(upload_length),
                                conflict_offset,
                            ),
                            res,
                        )
                        .await?;
                        return Ok(None);
                    }
                    Err(error) => return Err(error.into()),
                };
                if target_revision(owner_id, &canonical_relative_path, target.identity)
                    != Some(expected)
                {
                    self.render_upload_target_conflict(
                        path,
                        owner_id,
                        UploadErrorContext::new(
                            upload_id,
                            conflict_state,
                            Some(upload_length),
                            conflict_offset,
                        ),
                        res,
                    )
                    .await?;
                    return Ok(None);
                }
                (target.identity, target.metadata)
            }
        };
        if upload_deadline_expired(
            deadline,
            res,
            upload_id,
            "Upload deadline exceeded while inspecting the target",
        )? {
            return Ok(None);
        }
        let session_checkpoint = if resume {
            let Some(checkpoint) = existing_record else {
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(upload_id, UploadPublicState::NotSeen, None, None),
                    StatusCode::NOT_FOUND,
                    ErrorCode::UPLOAD_SESSION_NOT_FOUND,
                    "Not Found",
                    RecoveryAdvice::RetryWithNewId,
                )?;
                return Ok(None);
            };
            Some(checkpoint)
        } else {
            None
        };
        if upload_deadline_expired(
            deadline,
            res,
            upload_id,
            "Upload deadline exceeded while preparing the checkpoint",
        )? {
            return Ok(None);
        }
        let initial_offset = upload_offset.unwrap_or_default();
        if let Some(checkpoint) = session_checkpoint.as_ref() {
            if checkpoint.upload_length != upload_length {
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::Running,
                        Some(checkpoint.upload_length),
                        Some(checkpoint.durable_offset),
                    ),
                    StatusCode::CONFLICT,
                    ErrorCode::UPLOAD_LENGTH_CHANGED,
                    format!(
                        "Upload length changed: expected {}, received {upload_length}",
                        checkpoint.upload_length,
                    ),
                    RecoveryAdvice::QueryUpload,
                )?;
                return Ok(None);
            }
            if checkpoint.durable_offset != initial_offset {
                apply_upload_problem(
                    res,
                    UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::Running,
                        Some(checkpoint.upload_length),
                        Some(checkpoint.durable_offset),
                    ),
                    StatusCode::CONFLICT,
                    ErrorCode::UPLOAD_OFFSET_CHANGED,
                    "Upload offset changed; query it again",
                    RecoveryAdvice::QueryUpload,
                )?;
                return Ok(None);
            }
        }
        let remaining = upload_length.checked_sub(initial_offset).ok_or_else(|| {
            anyhow!("Upload offset {initial_offset} exceeds total length {upload_length}")
        })?;
        if request_body_length.is_some_and(|body_length| body_length > remaining) {
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
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::UPLOAD_BODY_EXCEEDS_REMAINING_LENGTH,
                "Request body exceeds declared remaining upload length",
                if resume {
                    RecoveryAdvice::ResumeUpload
                } else {
                    RecoveryAdvice::RetryWithNewId
                },
            )?;
            return Ok(None);
        }

        let mut created_ancestors = None;
        let mut pre_reserved_space = None;
        if !resume {
            let reservation_anchor = self
                .content
                .rooted_fs
                .open_nearest_existing_parent(&upload_path)
                .await?;
            let Some(space_lease) = self
                .admission
                .disk_space
                .reserve_allocated_async(
                    &reservation_anchor,
                    remaining,
                    UPLOAD_RESERVATION_METADATA_BYTES,
                    self.content.args.min_free_space,
                )
                .await?
            else {
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
                    "Insufficient protected free disk space",
                    RecoveryAdvice::RetryWithNewId,
                )?;
                return Ok(None);
            };
            pre_reserved_space = Some(space_lease);
            if upload_deadline_expired(
                deadline,
                res,
                upload_id,
                "Upload deadline exceeded while reserving disk space",
            )? {
                return Ok(None);
            }

            let created = self.content.rooted_fs.ensure_parent(&upload_path).await?;
            created_ancestors = Some(created);
            active_upload_files = match self.track_active_upload_files(&upload_path, deadline).await
            {
                Ok(lease) => Some(lease),
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    rollback_upload_ancestors(&self.content.rooted_fs, &mut created_ancestors)
                        .await?;
                    upload_deadline_expired(
                        deadline,
                        res,
                        upload_id,
                        "Upload deadline exceeded during preparation",
                    )?;
                    return Ok(None);
                }
                Err(error) => {
                    rollback_upload_ancestors(&self.content.rooted_fs, &mut created_ancestors)
                        .await?;
                    return Err(error.into());
                }
            };
            if upload_deadline_expired(
                deadline,
                res,
                upload_id,
                "Upload deadline exceeded during preparation",
            )? {
                rollback_upload_ancestors(&self.content.rooted_fs, &mut created_ancestors).await?;
                return Ok(None);
            }
        }
        let active_upload_files =
            active_upload_files.expect("every upload has an active-session lease");

        let (file, status) = match upload_offset {
            None => {
                let mut file = match create_upload_temp(&self.content.rooted_fs, &upload_path).await
                {
                    Ok(file) => file,
                    Err(error)
                        if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            error.kind() == std::io::ErrorKind::AlreadyExists
                        }) =>
                    {
                        rollback_upload_ancestors(&self.content.rooted_fs, &mut created_ancestors)
                            .await?;
                        apply_upload_problem(
                            res,
                            UploadErrorContext::new(
                                upload_id,
                                UploadPublicState::NotSeen,
                                Some(upload_length),
                                None,
                            ),
                            StatusCode::CONFLICT,
                            ErrorCode::UPLOAD_STAGE_CONFLICT,
                            "Upload staging path is already occupied; retry with a new upload ID",
                            RecoveryAdvice::RetryWithNewId,
                        )?;
                        return Ok(None);
                    }
                    Err(error) => {
                        rollback_upload_ancestors(&self.content.rooted_fs, &mut created_ancestors)
                            .await?;
                        return Err(error);
                    }
                };
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
                            message: "Upload deadline exceeded while preparing the staging file",
                            created_ancestors: created_ancestors.take(),
                        },
                    )
                    .await?;
                    return Ok(None);
                }
                if let Err(error) = self
                    .state
                    .upload_records
                    .persist_initial_checkpoint(
                        &mut file,
                        UploadRecordContext::new(
                            owner_id,
                            upload_id,
                            path,
                            &upload_path,
                            upload_length,
                        )
                        .with_target_revision(requested_revision),
                        0,
                    )
                    .await
                {
                    let discard_result = self
                        .state
                        .upload_records
                        .discard_unrecorded_stage(&file, &upload_path)
                        .await;
                    drop(file);
                    let reset_result = self
                        .state
                        .upload_records
                        .reset(owner_id, upload_id, path, &upload_path)
                        .await;
                    let rollback_result =
                        rollback_upload_ancestors(&self.content.rooted_fs, &mut created_ancestors)
                            .await;
                    discard_result?;
                    reset_result?;
                    rollback_result?;
                    return Err(error);
                }
                (file, StatusCode::CREATED)
            }
            Some(offset) => {
                let checkpoint = session_checkpoint.expect("loaded upload checkpoint");
                debug_assert_eq!(checkpoint.upload_length, upload_length);
                debug_assert_eq!(checkpoint.durable_offset, offset);
                // Reopen and validate the exact inode stored at the durable
                // checkpoint, then classify and mutate that same descriptor.
                // UUID knowledge is never an authorization or ownership
                // boundary for the physical staging pathname.
                let Some(mut file) = self
                    .state
                    .upload_records
                    .open_resumable_stage(
                        UploadRecordContext::new(
                            owner_id,
                            upload_id,
                            path,
                            &upload_path,
                            upload_length,
                        ),
                        offset,
                        checkpoint.state,
                    )
                    .await?
                else {
                    apply_upload_problem(
                        res,
                        UploadErrorContext::new(
                            upload_id,
                            if checkpoint.state == UploadRecordState::AwaitingConfirmation {
                                UploadPublicState::AwaitingConfirmation
                            } else {
                                UploadPublicState::Running
                            },
                            Some(upload_length),
                            Some(offset),
                        ),
                        StatusCode::CONFLICT,
                        ErrorCode::UPLOAD_STAGE_INVALID,
                        "Upload staging file is invalid; restart the upload",
                        RecoveryAdvice::QueryUpload,
                    )?;
                    return Ok(None);
                };
                let metadata = file.metadata().await?;
                if metadata.len() > offset {
                    file.set_len(offset).await?;
                }
                file.seek(SeekFrom::Start(offset)).await?;
                if checkpoint.target_revision != requested_revision {
                    let record = UploadRecordContext::new(
                        owner_id,
                        upload_id,
                        path,
                        &upload_path,
                        upload_length,
                    )
                    .with_target_revision(requested_revision);
                    match checkpoint.state {
                        UploadRecordState::Running => {
                            self.state
                                .upload_records
                                .persist_checkpoint(&mut file, record, offset)
                                .await?;
                        }
                        UploadRecordState::AwaitingConfirmation => {
                            self.state
                                .upload_records
                                .persist_awaiting_confirmation(&mut file, record)
                                .await?;
                        }
                        _ => unreachable!("only resumable upload states reopen a stage"),
                    }
                }
                (file, StatusCode::NO_CONTENT)
            }
        };
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
                    message: "Upload deadline exceeded while preparing the staging file",
                    created_ancestors: created_ancestors.take(),
                },
            )
            .await?;
            return Ok(None);
        }

        let reservation = if let Some(space_lease) = pre_reserved_space.take() {
            Some(space_lease)
        } else {
            self.admission
                .disk_space
                .reserve_allocated_async(
                    &file,
                    remaining,
                    UPLOAD_RESERVATION_METADATA_BYTES,
                    self.content.args.min_free_space,
                )
                .await?
        };
        let Some(mut space_lease) = reservation else {
            drop(file);
            if !resume {
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
                StatusCode::INSUFFICIENT_STORAGE,
                ErrorCode::UPLOAD_INSUFFICIENT_STORAGE,
                "Insufficient protected free disk space",
                if resume {
                    RecoveryAdvice::ResumeUpload
                } else {
                    RecoveryAdvice::RetryWithNewId
                },
            )?;
            return Ok(None);
        };
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
                    "Protected free disk space could not be confirmed after staging preparation",
                )
                .await?;
                return Ok(None);
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
                    message: "Upload deadline exceeded while reserving disk space",
                    created_ancestors: created_ancestors.take(),
                },
            )
            .await?;
            return Ok(None);
        }

        Ok(Some(UploadTransaction {
            target_path: path,
            upload_path,
            owner_id,
            mode,
            upload_id,
            upload_length,
            deadline,
            target_identity,
            target_revision: requested_revision,
            target_metadata,
            created_ancestors,
            file: Some(file),
            space_lease,
            success_status: status,
            _path_lease: path_lease,
            _active_upload_files: active_upload_files,
        }))
    }
}

pub(super) async fn create_upload_temp(rooted_fs: &RootedFs, path: &Path) -> Result<fs::File> {
    let (file, _) = rooted_fs.create_private_new(path).await?;
    Ok(file)
}
