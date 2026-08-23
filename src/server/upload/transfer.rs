use super::{
    failure::{UploadTimeout, finish_timed_out_upload},
    *,
};

#[derive(Debug)]
pub(super) enum UploadTransferError {
    Io(io::Error),
    IdleTimeout,
    TotalTimeout,
    ExcessBody,
    InsufficientStorage,
}

impl fmt::Display for UploadTransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::IdleTimeout => formatter.write_str("upload body idle timeout"),
            Self::TotalTimeout => formatter.write_str("upload body total timeout"),
            Self::ExcessBody => {
                formatter.write_str("request body exceeds the declared remaining upload length")
            }
            Self::InsufficientStorage => {
                formatter.write_str("upload would consume the protected free disk space")
            }
        }
    }
}

impl std::error::Error for UploadTransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

fn partial_upload_is_checkpointable(
    partial_size: u64,
    initial_offset: u64,
    upload_length: u64,
    resume: bool,
) -> bool {
    partial_size >= initial_offset
        && partial_size <= upload_length
        && (resume || partial_size >= RESUMABLE_UPLOAD_MIN_SIZE)
}

impl Server {
    /// Consumes the request body while the transaction retains all rollback
    /// state. A successful transfer advances to the pre-commit stage.
    pub(super) async fn transfer_upload_body(
        &self,
        upload: UploadTransaction<'_>,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        let UploadTransaction {
            target_path: path,
            upload_path,
            owner_id,
            mode,
            upload_id,
            upload_length,
            deadline,
            target_identity,
            target_revision,
            checkpoint_state,
            target_metadata,
            mut created_ancestors,
            file,
            mut space_lease,
            success_status: status,
            _path_lease: path_lease,
            _active_upload_files: active_upload_files,
        } = upload;
        let resume = mode.is_resume();
        let awaiting_confirmation =
            checkpoint_state == Some(UploadRecordState::AwaitingConfirmation);
        let initial_offset = mode.offset().unwrap_or_default();
        let remaining = upload_length
            .checked_sub(initial_offset)
            .expect("validated upload offset");
        let mut file = file.expect("prepared upload owns a file");

        let transfer_result = receive_upload_body(
            IncomingStream::new(req.into_body()),
            &mut file,
            &mut space_lease,
            UploadTransferOptions {
                remaining,
                minimum_free: self.content.args.min_free_space,
                idle_timeout: Duration::from_secs(self.content.args.upload_idle_timeout),
                total_deadline: deadline,
                force_shutdown: &self.lifecycle.force_shutdown,
            },
        )
        .await;
        if let Err(err) = transfer_result {
            if matches!(&err, UploadTransferError::TotalTimeout) {
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
                        message: "Upload deadline exceeded; the final result is unknown",
                        resume_offset: mode.offset(),
                        created_ancestors: created_ancestors.take(),
                    },
                )
                .await?;
                return Ok(());
            }
            let partial_size = match file.metadata().await {
                Ok(metadata) => metadata.len(),
                Err(error) if resume => return Err(error.into()),
                Err(_) => 0,
            };

            match err {
                UploadTransferError::ExcessBody => {
                    if awaiting_confirmation {
                        drop(file);
                    } else if resume {
                        file.set_len(initial_offset).await?;
                        sync_file_to_storage(&file).await?;
                        drop(file);
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
                    apply_upload_problem(
                        res,
                        UploadErrorContext::new(
                            upload_id,
                            if awaiting_confirmation {
                                UploadPublicState::AwaitingConfirmation
                            } else if resume {
                                UploadPublicState::Running
                            } else {
                                UploadPublicState::Rejected
                            },
                            Some(upload_length),
                            resume.then_some(initial_offset),
                        ),
                        StatusCode::PAYLOAD_TOO_LARGE,
                        ErrorCode::UPLOAD_BODY_EXCEEDS_REMAINING_LENGTH,
                        "Request body exceeds declared remaining upload length",
                        if awaiting_confirmation {
                            RecoveryAdvice::QueryUpload
                        } else if resume {
                            RecoveryAdvice::ResumeUpload
                        } else {
                            RecoveryAdvice::RetryWithNewId
                        },
                    )?;
                    return Ok(());
                }
                UploadTransferError::InsufficientStorage => {
                    file.set_len(initial_offset).await?;
                    sync_file_to_storage(&file).await?;
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
                    return Ok(());
                }
                UploadTransferError::IdleTimeout => {
                    let keep_for_resume = partial_upload_is_checkpointable(
                        partial_size,
                        initial_offset,
                        upload_length,
                        resume,
                    );
                    if keep_for_resume {
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
                                partial_size,
                            )
                            .await?;
                        res.headers_mut().insert(
                            UPLOAD_OFFSET_HEADER,
                            HeaderValue::from_str(&partial_size.to_string())?,
                        );
                    } else if !resume {
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
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "resumed upload stage length no longer contains its durable checkpoint",
                        )
                        .into());
                    }
                    let state = if keep_for_resume {
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
                            keep_for_resume.then_some(partial_size),
                        ),
                        StatusCode::REQUEST_TIMEOUT,
                        ErrorCode::UPLOAD_IDLE_TIMEOUT,
                        err.to_string(),
                        if keep_for_resume {
                            RecoveryAdvice::ResumeUpload
                        } else {
                            RecoveryAdvice::RetryWithNewId
                        },
                    )?;
                    return Ok(());
                }
                UploadTransferError::Io(err) => {
                    let keep_for_resume = partial_upload_is_checkpointable(
                        partial_size,
                        initial_offset,
                        upload_length,
                        resume,
                    );
                    if keep_for_resume {
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
                                partial_size,
                            )
                            .await?;
                    } else if !resume {
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
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "resumed upload stage length no longer contains its durable checkpoint",
                        )
                        .into());
                    }
                    return Err(err.into());
                }
                UploadTransferError::TotalTimeout => unreachable!("handled before metadata I/O"),
            }
        }

        self.prepare_transferred_upload(
            TransferredUpload {
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
                created_ancestors,
                file,
                space_lease,
                success_status: status,
                _path_lease: path_lease,
                _active_upload_files: active_upload_files,
            },
            res,
        )
        .await
    }
}

pub(super) struct UploadTransferOptions<'a> {
    pub(super) remaining: u64,
    pub(super) minimum_free: u64,
    pub(super) idle_timeout: Duration,
    pub(super) total_deadline: Instant,
    pub(super) force_shutdown: &'a tokio_util::sync::CancellationToken,
}

pub(super) async fn receive_upload_body<S>(
    stream: S,
    file: &mut fs::File,
    space_lease: &mut DiskSpaceReservation,
    options: UploadTransferOptions<'_>,
) -> std::result::Result<(), UploadTransferError>
where
    S: Stream<Item = Result<Bytes>>,
{
    let UploadTransferOptions {
        mut remaining,
        minimum_free,
        idle_timeout,
        total_deadline,
        force_shutdown,
    } = options;
    let started = Instant::now();
    let mut idle_deadline = started.checked_add(idle_timeout).ok_or_else(|| {
        UploadTransferError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upload idle timeout is too large",
        ))
    })?;
    let mut bytes_until_space_check = DISK_SPACE_RECHECK_BYTES;
    pin_mut!(stream);

    loop {
        let deadline = total_deadline.min(idle_deadline);
        let mut stream_ref = stream.as_mut();
        let next_frame = stream_ref.try_next();
        let next = tokio::select! {
            biased;
            _ = force_shutdown.cancelled() => {
                return Err(UploadTransferError::Io(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "upload interrupted by forced shutdown",
                )));
            }
            result = timeout_at(deadline, next_frame) => {
                match result {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => {
                        return Err(UploadTransferError::Io(io::Error::other(error)));
                    }
                    Err(_) if Instant::now() >= total_deadline => {
                        return Err(UploadTransferError::TotalTimeout);
                    }
                    Err(_) => return Err(UploadTransferError::IdleTimeout),
                }
            }
        };

        let Some(chunk) = next else {
            return Ok(());
        };
        if chunk.is_empty() {
            continue;
        }

        let chunk_len = u64::try_from(chunk.len()).map_err(|_| {
            UploadTransferError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "upload chunk length does not fit in u64",
            ))
        })?;
        let accepted_bytes = remaining.min(chunk_len);
        if accepted_bytes > 0 {
            let accepted_len = usize::try_from(accepted_bytes).map_err(|_| {
                UploadTransferError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "accepted upload chunk length does not fit in usize",
                ))
            })?;
            // Do not cancel a Tokio file operation at the deadline. The outer
            // upload task reports `unknown` on time, while this future drains
            // the current write before its path lease can be released.
            write_upload_chunk(
                space_lease,
                file,
                &chunk[..accepted_len],
                minimum_free,
                &mut bytes_until_space_check,
            )
            .await?;
            remaining -= accepted_bytes;
            idle_deadline = Instant::now().checked_add(idle_timeout).ok_or_else(|| {
                UploadTransferError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "upload idle timeout is too large",
                ))
            })?;
            if Instant::now() >= total_deadline {
                return Err(UploadTransferError::TotalTimeout);
            }
        }
        if chunk_len > accepted_bytes {
            return Err(UploadTransferError::ExcessBody);
        }
    }
}

async fn write_upload_chunk(
    reservation: &mut DiskSpaceReservation,
    file: &mut fs::File,
    data: &[u8],
    minimum_free: u64,
    bytes_until_space_check: &mut u64,
) -> std::result::Result<(), UploadTransferError> {
    if data.is_empty() {
        return Ok(());
    }
    let data_len = u64::try_from(data.len()).map_err(|_| {
        UploadTransferError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upload chunk length does not fit in u64",
        ))
    })?;
    if data_len > reservation.remaining() {
        return Err(UploadTransferError::ExcessBody);
    }
    if data_len >= *bytes_until_space_check {
        if !reservation
            .reserved_space_is_available_async(file, minimum_free)
            .await
            .map_err(UploadTransferError::Io)?
        {
            return Err(UploadTransferError::InsufficientStorage);
        }
        *bytes_until_space_check = DISK_SPACE_RECHECK_BYTES;
    }
    file.write_all(data)
        .await
        .map_err(UploadTransferError::Io)?;
    reservation.consume(data_len);
    *bytes_until_space_check = bytes_until_space_check.saturating_sub(data_len);
    Ok(())
}
