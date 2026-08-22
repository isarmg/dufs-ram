use super::{
    Response, Server,
    browser_api::{SOURCE_REVISION_HEADER, apply_revision_header},
    identity::OwnerId,
    operation_registry::{
        OperationGuard, OperationOutcome, TrackedOperationError, apply_operation_outcome,
        set_operation_headers,
    },
    path_coordinator::PathLease,
    problem::{ApiError, ErrorCode, OperationProblemContext, RecoveryAdvice, render_problem},
    protocol::OperationPublicState,
    purge::{PreparePurge, PreparedPurge},
    rooted_fs::{CheckedTrashMove, DeleteIdentity, ReplacementTargetIdentity},
    router::MutationProgress,
    upload::{TargetRevision, target_revision},
};
use anyhow::Result;
use hyper::StatusCode;
use std::{path::Path, sync::Arc};

enum DeleteCommitOutcome {
    Succeeded,
    Rejected(OperationOutcome),
    TargetChanged(OperationOutcome, Option<TargetRevision>),
}

pub(super) struct DeleteRequest<'a> {
    pub(super) owner: &'a str,
    pub(super) path: &'a Path,
    pub(super) expected_revision_identity: ReplacementTargetIdentity,
    pub(super) expected_delete_identity: DeleteIdentity,
    pub(super) mutation: MutationProgress,
    pub(super) path_lease: PathLease,
    pub(super) operation: (uuid::Uuid, OperationGuard),
}

impl Server {
    pub(super) async fn handle_delete(
        self: &Arc<Self>,
        request: DeleteRequest<'_>,
        res: &mut Response,
    ) -> Result<()> {
        let DeleteRequest {
            owner,
            path,
            expected_revision_identity,
            expected_delete_identity,
            mutation,
            path_lease,
            operation,
        } = request;
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
                let prepared = match server
                    .prepare_purge_with_identity(&owner, &path, expected_delete_identity)
                    .await
                {
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
                        if let Some(operation) = operation.take() {
                            operation.complete(outcome).await?;
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
                    let trash = match server
                        .content
                        .rooted_fs
                        .move_to_trash_with_expected_identity_outcome(
                            &path,
                            trash_id,
                            source_identity,
                            Some(expected_revision_identity),
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
                                StatusCode::PRECONDITION_FAILED,
                                TrackedOperationError::DeleteTargetChanged,
                            );
                            if let Some(operation) = operation.take() {
                                operation.complete(outcome).await?;
                            }
                            let relative =
                                server.content.rooted_fs.state_relative_path(&path)?;
                            let current_revision = match server
                                .content
                                .rooted_fs
                                .replacement_identity(&path)
                                .await
                            {
                                Ok(identity) => target_revision(
                                    OwnerId::persistent(&owner),
                                    &relative,
                                    identity,
                                ),
                                Err(error) => {
                                    warn!(
                                        "Failed to inspect the changed DELETE target for a response revision error={error:#}"
                                    );
                                    None
                                }
                            };
                            return Ok(DeleteCommitOutcome::TargetChanged(
                                outcome,
                                current_revision,
                            ));
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
                    if !server
                        .state
                        .state_store
                        .mark_purge_job_ready(key, trash.trash_revision())
                        .await?
                    {
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
            DeleteCommitOutcome::TargetChanged(outcome, current_revision) => {
                apply_revision_header(res, SOURCE_REVISION_HEADER, current_revision)?;
                apply_operation_outcome(res, operation_id, outcome, false)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Args,
        auth::AuthConfig,
        server::{
            ServerLifecycle,
            operation_registry::{BeginOperation, OperationFingerprint, OperationStatus},
        },
    };
    use rusqlite::Connection;
    use std::os::unix::fs::PermissionsExt;

    const TEST_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

    fn server(root: &Path) -> (Arc<Server>, assert_fs::TempDir) {
        let state_dir = assert_fs::TempDir::new().unwrap();
        std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = Server::init_with_lifecycle(
            Args {
                serve_path: root.to_path_buf(),
                state_dir: Some(state_dir.path().to_path_buf()),
                auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
                ..Args::default()
            },
            ServerLifecycle::new(),
        )
        .unwrap();
        (Arc::new(server), state_dir)
    }

    #[tokio::test]
    async fn purge_prepare_and_rejection_write_failures_do_not_report_a_known_outcome() {
        let root = assert_fs::TempDir::new().unwrap();
        let (server, state_dir) = server(root.path());
        let owner = "user";
        let path = root.path().join("kept.txt");
        std::fs::write(&path, "kept").unwrap();
        let expected_revision_identity = server
            .content
            .rooted_fs
            .replacement_identity(&path)
            .await
            .unwrap();
        let expected_delete_identity = expected_revision_identity.delete_identity().unwrap();
        let operation_id = uuid::Uuid::new_v4();
        let BeginOperation::Started(operation) = server
            .state
            .operation_registry
            .begin(
                owner,
                operation_id,
                OperationFingerprint::new(&[b"DELETE rejection persistence test"]),
            )
            .await
            .unwrap()
        else {
            panic!("test operation was not admitted");
        };

        let connection = Connection::open(state_dir.path().join("state.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_purge_insert
                   BEFORE INSERT ON purge_jobs
                   BEGIN
                     SELECT RAISE(ABORT, 'injected purge preparation failure');
                   END;
                 CREATE TRIGGER fail_operation_update
                   BEFORE UPDATE ON operations
                   BEGIN
                     SELECT RAISE(ABORT, 'injected operation completion failure');
                   END;",
            )
            .unwrap();

        let mutation = MutationProgress::default();
        mutation.mark_reserved();
        let path_lease = server
            .content
            .path_coordinator
            .acquire([path.as_path()])
            .await;
        let mut response = Response::default();
        let error = server
            .handle_delete(
                DeleteRequest {
                    owner,
                    path: &path,
                    expected_revision_identity,
                    expected_delete_identity,
                    mutation: mutation.clone(),
                    path_lease,
                    operation: (operation_id, operation),
                },
                &mut response,
            )
            .await
            .expect_err("an unrecorded DELETE outcome must remain uncertain");

        assert!(
            format!("{error:#}").contains("injected operation completion failure"),
            "unexpected error: {error:#}"
        );
        assert!(mutation.outcome_can_be_unknown());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(std::fs::read(&path).unwrap(), b"kept");

        connection
            .execute_batch(
                "DROP TRIGGER fail_purge_insert;
                 DROP TRIGGER fail_operation_update;",
            )
            .unwrap();
        server.state.state_store.probe_readiness().await.unwrap();
        assert!(matches!(
            server
                .state
                .operation_registry
                .status(owner, operation_id)
                .await
                .unwrap(),
            OperationStatus::NotFound
        ));
    }
}
