use super::{
    Response, Server,
    operation_registry::{
        OperationGuard, OperationOutcome, TrackedOperationError, apply_operation_outcome,
        set_operation_headers,
    },
    path_coordinator::PathLease,
    problem::{ApiError, ErrorCode, OperationProblemContext, RecoveryAdvice, render_problem},
    protocol::OperationPublicState,
    purge::{PreparePurge, PreparedPurge},
    rooted_fs::CheckedTrashMove,
    router::MutationProgress,
};
use anyhow::Result;
use hyper::StatusCode;
use std::{path::Path, sync::Arc};

enum DeleteCommitOutcome {
    Succeeded,
    Rejected(OperationOutcome),
}

impl Server {
    pub(super) async fn handle_delete(
        self: &Arc<Self>,
        owner: &str,
        path: &Path,
        res: &mut Response,
        mutation: MutationProgress,
        path_lease: PathLease,
        operation: (uuid::Uuid, OperationGuard),
    ) -> Result<()> {
        let (operation_id, operation) = operation;
        match self.has_persisted_path_conflict(&[path]).await {
            Ok(false) => {}
            Ok(true) => {
                let outcome = OperationOutcome::failure(
                    StatusCode::CONFLICT,
                    TrackedOperationError::DeleteStateConflict,
                );
                operation.complete(outcome).await?;
                apply_operation_outcome(res, operation_id, outcome, false)?;
                return Ok(());
            }
            Err(error) => {
                error!("Failed to inspect durable state before delete error={error:#}");
                // Admission failed before creating this DELETE's Prepared
                // intent or marking its operation commit-started.
                drop(operation);
                set_operation_headers(res, operation_id, OperationPublicState::Rejected);
                render_problem(
                    res,
                    &ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorCode::PURGE_STATE_UNAVAILABLE,
                        "Delete state storage is temporarily unavailable",
                    )
                    .with_recovery(RecoveryAdvice::RetryAfterSeconds(1))
                    .with_operation(OperationProblemContext::new(
                        operation_id.hyphenated().to_string(),
                        OperationPublicState::Rejected,
                        None,
                    )),
                )?;
                return Ok(());
            }
        }

        let server = self.clone();
        let owner = owner.to_owned();
        let path = path.to_path_buf();
        let operation = Some(operation);
        let commit_outcome = self
            .run_operation_commit(mutation, async move {
                let mut operation = operation;
                let prepared = match server.prepare_purge(&owner, &path).await {
                    Ok(PreparePurge::Prepared(prepared)) => prepared,
                    Ok(PreparePurge::Full) => {
                        let outcome = OperationOutcome::failure(
                            StatusCode::SERVICE_UNAVAILABLE,
                            TrackedOperationError::PurgeBacklogFull,
                        );
                        if let Some(operation) = operation.take() {
                            operation.complete(outcome).await?;
                        }
                        return Ok(DeleteCommitOutcome::Rejected(outcome));
                    }
                    Err(error) => {
                        error!("Failed to prepare durable purge state error={error:#}");
                        let outcome = OperationOutcome::failure(
                            StatusCode::SERVICE_UNAVAILABLE,
                            TrackedOperationError::PurgeStateUnavailable,
                        );
                        if let Some(operation) = operation.take()
                            && let Err(store_error) = operation.complete(outcome).await
                        {
                            error!(
                                "Failed to persist known delete rejection error={store_error:#}"
                            );
                        }
                        return Ok(DeleteCommitOutcome::Rejected(outcome));
                    }
                };
                let PreparedPurge {
                    key,
                    trash_id,
                    source_identity,
                } = prepared;
                let commit_result = async {
                    let _path_lease = path_lease;
                    if let Some(operation) = operation.as_mut() {
                        operation.mark_commit_started().await?;
                    }
                    let _trash = match server
                        .content
                        .rooted_fs
                        .move_to_trash_with_expected_identity_outcome(
                            &path,
                            trash_id,
                            source_identity,
                        )
                        .await
                    {
                        CheckedTrashMove::Moved(trash) => trash,
                        CheckedTrashMove::TargetChanged => {
                            if !server.state.state_store.remove_purge_job(key).await? {
                                anyhow::bail!(
                                    "durable purge intent disappeared before known rejection"
                                );
                            }
                            let outcome = OperationOutcome::failure(
                                StatusCode::CONFLICT,
                                TrackedOperationError::DeleteTargetChanged,
                            );
                            if let Some(operation) = operation.take() {
                                operation.complete(outcome).await?;
                            }
                            return Ok(DeleteCommitOutcome::Rejected(outcome));
                        }
                        CheckedTrashMove::NotMoved(error) => {
                            if !server.state.state_store.remove_purge_job(key).await? {
                                anyhow::bail!(
                                    "durable purge intent disappeared before failed rename cleanup"
                                );
                            }
                            error!("Delete rename failed before commit error={error:#}");
                            let outcome = OperationOutcome::failure(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                TrackedOperationError::DeleteNotCommitted,
                            );
                            if let Some(operation) = operation.take() {
                                operation.complete(outcome).await?;
                            }
                            return Ok(DeleteCommitOutcome::Rejected(outcome));
                        }
                        CheckedTrashMove::DurabilityUnknown(error) => {
                            return Err(error.into());
                        }
                    };
                    if !server.state.state_store.mark_purge_job_ready(key).await? {
                        anyhow::bail!("durable purge intent disappeared after filesystem rename");
                    }
                    if let Some(operation) = operation {
                        operation
                            .complete(OperationOutcome::success(StatusCode::NO_CONTENT))
                            .await?;
                    }
                    Ok::<_, anyhow::Error>(DeleteCommitOutcome::Succeeded)
                }
                .await;
                match commit_result {
                    Ok(outcome) => {
                        if matches!(&outcome, DeleteCommitOutcome::Succeeded) {
                            server.notify_purge_worker();
                        }
                        Ok(outcome)
                    }
                    Err(error) => {
                        // The path lease owned by the inner future has been
                        // released, so reconciliation can safely acquire the
                        // same semantic paths. Keeping this inside the tracked
                        // task means a disconnected client cannot strand a
                        // Prepared intent.
                        server.reconcile_prepared_purge_key(key).await;
                        Err(error)
                    }
                }
            })
            .await?;
        match commit_outcome {
            DeleteCommitOutcome::Succeeded => {
                apply_operation_outcome(
                    res,
                    operation_id,
                    OperationOutcome::success(StatusCode::NO_CONTENT),
                    false,
                )?;
            }
            DeleteCommitOutcome::Rejected(outcome) => {
                apply_operation_outcome(res, operation_id, outcome, false)?;
            }
        }
        Ok(())
    }
}
