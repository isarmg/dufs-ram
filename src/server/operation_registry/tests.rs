use super::*;
use http_body_util::BodyExt as _;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Barrier;

fn fingerprint(value: &[u8]) -> OperationFingerprint {
    OperationFingerprint::new(&[value])
}

#[test]
fn tracked_operation_errors_round_trip_stable_wire_metadata() {
    let expected = [
        (
            TrackedOperationError::InvalidJson,
            "invalid_json",
            "Invalid JSON request",
        ),
        (
            TrackedOperationError::InvalidPath,
            "invalid_path",
            "Invalid path",
        ),
        (
            TrackedOperationError::PathExists,
            "path_exists",
            "Path already exists",
        ),
        (
            TrackedOperationError::InvalidSourcePath,
            "invalid_source_path",
            "Invalid source path",
        ),
        (
            TrackedOperationError::InvalidDestinationPath,
            "invalid_destination_path",
            "Invalid destination path",
        ),
        (
            TrackedOperationError::SourceEqualsDestination,
            "source_equals_destination",
            "Source and destination must differ",
        ),
        (
            TrackedOperationError::InvalidMovePath,
            "invalid_move_path",
            "Invalid move path",
        ),
        (
            TrackedOperationError::InvalidRenameName,
            "invalid_rename_name",
            "Rename name must be one valid path segment",
        ),
        (
            TrackedOperationError::InvalidRenamePath,
            "invalid_rename_path",
            "Invalid rename path",
        ),
        (
            TrackedOperationError::SourceNotFound,
            "source_not_found",
            "Source not found",
        ),
        (
            TrackedOperationError::DirectoryIntoItself,
            "directory_into_itself",
            "A directory cannot be moved into itself",
        ),
        (
            TrackedOperationError::DestinationExists,
            "destination_exists",
            "Destination already exists",
        ),
        (
            TrackedOperationError::DestinationDirectoryNotFound,
            "destination_directory_not_found",
            "Destination directory not found",
        ),
        (
            TrackedOperationError::DestinationNotDirectory,
            "destination_not_directory",
            "Destination path is not a directory",
        ),
        (
            TrackedOperationError::DirectoryOverwriteForbidden,
            "directory_overwrite_forbidden",
            "Directories cannot be overwritten",
        ),
        (
            TrackedOperationError::MkdirStateConflict,
            "mkdir_state_conflict",
            "Directory path conflicts with an active upload or pending delete",
        ),
        (
            TrackedOperationError::MoveStateConflict,
            "move_state_conflict",
            "Source or destination conflicts with an active upload or pending delete",
        ),
        (
            TrackedOperationError::RenameStateConflict,
            "rename_state_conflict",
            "Rename source or destination conflicts with an active upload or pending delete",
        ),
        (
            TrackedOperationError::TargetNotFound,
            "target_not_found",
            "Target not found",
        ),
        (
            TrackedOperationError::DeleteStateConflict,
            "delete_state_conflict",
            "Target conflicts with an active upload or pending delete",
        ),
        (
            TrackedOperationError::PurgeBacklogFull,
            "purge_backlog_full",
            "Delete backlog is temporarily full",
        ),
        (
            TrackedOperationError::PurgeStateUnavailable,
            "purge_state_unavailable",
            "Delete state storage is temporarily unavailable",
        ),
        (
            TrackedOperationError::DeleteTargetChanged,
            "delete_target_changed",
            "Delete target changed before commit",
        ),
        (
            TrackedOperationError::DeleteNotCommitted,
            "delete_not_committed",
            "Delete was not committed; refresh the target before retrying",
        ),
        (
            TrackedOperationError::OutcomeUncertain,
            "outcome_uncertain",
            "Operation outcome is uncertain; inspect the target before trying again",
        ),
    ];

    for (error, code, detail) in expected {
        assert_eq!(TrackedOperationError::from_wire_name(code), Some(error));
        assert_eq!(error.code().as_str(), code);
        assert_eq!(error.detail(), detail);
    }
    assert_eq!(
        TrackedOperationError::OutcomeUncertain.recovery(),
        RecoveryAdvice::QueryJob
    );
    assert_eq!(
        TrackedOperationError::InvalidPath.recovery(),
        RecoveryAdvice::None
    );
    assert_eq!(TrackedOperationError::from_wire_name("invalid-json"), None);
}

#[tokio::test]
async fn same_request_is_running_then_replayed_and_mismatch_conflicts() {
    let registry = OperationRegistry::temporary_for_test().unwrap();
    let id = Uuid::new_v4();
    let BeginOperation::Started(guard) = registry
        .begin("alice", id, fingerprint(b"one"))
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    assert!(matches!(
        registry
            .begin("alice", id, fingerprint(b"one"))
            .await
            .unwrap(),
        BeginOperation::Running
    ));
    assert!(matches!(
        registry
            .begin("alice", id, fingerprint(b"two"))
            .await
            .unwrap(),
        BeginOperation::Conflict
    ));

    let outcome = OperationOutcome::success(StatusCode::NO_CONTENT);
    guard.complete(outcome).await.unwrap();
    assert!(matches!(
        registry.begin("alice", id, fingerprint(b"one")).await.unwrap(),
        BeginOperation::Replay(value) if value == outcome
    ));
}

#[tokio::test]
async fn registry_is_owner_scoped_and_status_does_not_cross_accounts() {
    let registry = OperationRegistry::temporary_for_test().unwrap();
    let id = Uuid::new_v4();
    let BeginOperation::Started(guard) = registry
        .begin("alice", id, fingerprint(b"request"))
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    assert!(matches!(
        registry.status("bob", id).await.unwrap(),
        OperationStatus::NotFound
    ));
    assert!(matches!(
        registry
            .begin("bob", id, fingerprint(b"other"))
            .await
            .unwrap(),
        BeginOperation::Started(_)
    ));
    guard
        .complete(OperationOutcome::success(StatusCode::CREATED))
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_retries_cannot_start_a_second_operation() {
    let registry = OperationRegistry::temporary_for_test().unwrap();
    let id = Uuid::new_v4();
    let BeginOperation::Started(guard) = registry
        .begin("alice", id, fingerprint(b"request"))
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    let barrier = Arc::new(Barrier::new(9));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let registry = registry.clone();
        let barrier = barrier.clone();
        workers.push(tokio::spawn(async move {
            barrier.wait().await;
            matches!(
                registry
                    .begin("alice", id, fingerprint(b"request"))
                    .await
                    .unwrap(),
                BeginOperation::Running
            )
        }));
    }
    barrier.wait().await;
    for worker in workers {
        assert!(worker.await.unwrap());
    }
    guard
        .complete(OperationOutcome::success(StatusCode::NO_CONTENT))
        .await
        .unwrap();
}

#[tokio::test]
async fn bounded_registry_rejects_new_work_when_all_entries_are_running() {
    let registry = OperationRegistry::with_limits(1, 1, Duration::from_secs(60));
    let BeginOperation::Started(_guard) = registry
        .begin("alice", Uuid::new_v4(), fingerprint(b"one"))
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    assert!(matches!(
        registry
            .begin("alice", Uuid::new_v4(), fingerprint(b"two"))
            .await
            .unwrap(),
        BeginOperation::Full
    ));
}

#[tokio::test]
async fn capacity_pressure_never_evicts_an_unexpired_completed_result() {
    let registry = OperationRegistry::with_limits(1, 1, Duration::from_secs(60));
    let completed_id = Uuid::new_v4();
    let completed_fingerprint = fingerprint(b"completed");
    let BeginOperation::Started(guard) = registry
        .begin("alice", completed_id, completed_fingerprint)
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    let outcome = OperationOutcome::success(StatusCode::NO_CONTENT);
    guard.complete(outcome).await.unwrap();

    assert!(matches!(
        registry
            .begin("alice", Uuid::new_v4(), fingerprint(b"new"))
            .await
            .unwrap(),
        BeginOperation::Full
    ));
    assert!(matches!(
        registry
            .begin("alice", completed_id, completed_fingerprint)
            .await
            .unwrap(),
        BeginOperation::Replay(replayed) if replayed == outcome
    ));
}

#[tokio::test]
async fn completed_entries_expire() {
    let registry = OperationRegistry::with_limits(1, 1, Duration::ZERO);
    let id = Uuid::new_v4();
    let BeginOperation::Started(guard) = registry
        .begin("alice", id, fingerprint(b"one"))
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    guard
        .complete(OperationOutcome::success(StatusCode::NO_CONTENT))
        .await
        .unwrap();
    assert!(matches!(
        registry.status("alice", id).await.unwrap(),
        OperationStatus::NotFound
    ));
    assert!(matches!(
        registry
            .begin("alice", Uuid::new_v4(), fingerprint(b"replacement"))
            .await
            .unwrap(),
        BeginOperation::Started(_)
    ));
}

#[tokio::test]
async fn running_entries_never_expire_while_the_guard_is_alive() {
    let registry = OperationRegistry::with_limits(2, 2, Duration::ZERO);
    let running_id = Uuid::new_v4();
    let BeginOperation::Started(_running_guard) = registry
        .begin("alice", running_id, fingerprint(b"running"))
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };

    let completed_id = Uuid::new_v4();
    let BeginOperation::Started(completed_guard) = registry
        .begin("alice", completed_id, fingerprint(b"completed"))
        .await
        .unwrap()
    else {
        panic!("second request must start");
    };
    completed_guard
        .complete(OperationOutcome::success(StatusCode::NO_CONTENT))
        .await
        .unwrap();

    assert!(matches!(
        registry.status("alice", running_id).await.unwrap(),
        OperationStatus::Running
    ));
    assert!(matches!(
        registry
            .begin("alice", running_id, fingerprint(b"running"))
            .await
            .unwrap(),
        BeginOperation::Running
    ));
}

#[tokio::test]
async fn dropping_a_reserved_guard_allows_a_safe_retry() {
    let registry = OperationRegistry::temporary_for_test().unwrap();
    let id = Uuid::new_v4();
    let BeginOperation::Started(guard) = registry
        .begin("alice", id, fingerprint(b"request"))
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    drop(guard);

    assert!(matches!(
        registry.status("alice", id).await.unwrap(),
        OperationStatus::NotFound
    ));
    assert!(matches!(
        registry
            .begin("alice", id, fingerprint(b"request"))
            .await
            .unwrap(),
        BeginOperation::Started(_)
    ));
}

#[tokio::test]
async fn dropping_after_commit_started_records_an_uncertain_outcome() {
    let registry = OperationRegistry::temporary_for_test().unwrap();
    let id = Uuid::new_v4();
    let BeginOperation::Started(mut guard) = registry
        .begin("alice", id, fingerprint(b"request"))
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    guard.mark_commit_started().await.unwrap();
    drop(guard);

    assert!(matches!(
        registry.status("alice", id).await.unwrap(),
        OperationStatus::Completed(outcome)
            if outcome == OperationOutcome::uncertain()
                && outcome.public_state() == OperationPublicState::Unknown
    ));
    assert!(matches!(
        registry
            .begin("alice", id, fingerprint(b"request"))
            .await
            .unwrap(),
        BeginOperation::Replay(outcome) if outcome == OperationOutcome::uncertain()
    ));
}

#[tokio::test]
async fn per_owner_capacity_preserves_registry_space_for_other_accounts() {
    let registry = OperationRegistry::with_limits(4, 2, Duration::from_secs(60));
    let first_id = Uuid::new_v4();
    let BeginOperation::Started(_first_guard) = registry
        .begin("alice", first_id, fingerprint(b"first"))
        .await
        .unwrap()
    else {
        panic!("first owner operation must start");
    };
    let BeginOperation::Started(_second_guard) = registry
        .begin("alice", Uuid::new_v4(), fingerprint(b"second"))
        .await
        .unwrap()
    else {
        panic!("second owner operation must start");
    };

    assert!(matches!(
        registry
            .begin("alice", Uuid::new_v4(), fingerprint(b"third"))
            .await
            .unwrap(),
        BeginOperation::Full
    ));
    assert!(matches!(
        registry
            .begin("alice", first_id, fingerprint(b"first"))
            .await
            .unwrap(),
        BeginOperation::Running
    ));
    assert!(matches!(
        registry
            .begin("bob", Uuid::new_v4(), fingerprint(b"other-owner"))
            .await
            .unwrap(),
        BeginOperation::Started(_)
    ));
}

#[tokio::test]
async fn persistent_registry_replays_completed_outcome_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let identity = super::super::state_store::RootIdentity::new(41, 42);
    let operation_id = Uuid::new_v4();
    let operation_fingerprint = fingerprint(b"persistent-operation");
    let outcome =
        OperationOutcome::failure(StatusCode::CONFLICT, TrackedOperationError::PathExists);

    let registry = OperationRegistry::open(&path, identity, RESULT_TTL).unwrap();
    let BeginOperation::Started(guard) = registry
        .begin("alice", operation_id, operation_fingerprint)
        .await
        .unwrap()
    else {
        panic!("first request must start");
    };
    guard.complete(outcome).await.unwrap();
    registry.store.clone().shutdown_for_test();
    drop(registry);

    let reopened = OperationRegistry::open(&path, identity, RESULT_TTL).unwrap();
    assert!(matches!(
        reopened
            .begin("alice", operation_id, operation_fingerprint)
            .await
            .unwrap(),
        BeginOperation::Replay(replayed) if replayed == outcome
    ));
}

#[tokio::test]
async fn uncertain_outcome_uses_stable_json_and_headers() {
    let id = Uuid::new_v4();
    let mut response = Response::default();
    apply_operation_outcome(&mut response, id, OperationOutcome::uncertain(), false).unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers().get(OPERATION_ID_HEADER).unwrap(),
        id.hyphenated().to_string().as_str()
    );
    assert_eq!(
        response.headers().get(OPERATION_STATE_HEADER).unwrap(),
        "unknown"
    );
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        json!({
            "type": "urn:dufs:problem:outcome_uncertain",
            "title": "Internal Server Error",
            "status": 500,
            "detail":
                "Operation outcome is uncertain; inspect the target before trying again",
            "operation_id": id.hyphenated().to_string(),
            "state": "unknown",
            "http_status": 500,
            "code": "outcome_uncertain",
            "recovery": "query_job"
        })
    );
}

#[tokio::test]
async fn full_registry_is_reported_as_a_known_rejection() {
    let id = Uuid::new_v4();
    let mut response = Response::default();
    apply_registry_full(&mut response, id).unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get(OPERATION_STATE_HEADER).unwrap(),
        "rejected"
    );
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        json!({
            "type": "urn:dufs:problem:operation_registry_full",
            "title": "Service Unavailable",
            "status": 503,
            "detail": "Operation registry is temporarily full",
            "operation_id": id.hyphenated().to_string(),
            "state": "rejected",
            "code": "operation_registry_full",
            "recovery": "retry",
            "retry_after": 1
        })
    );
}

#[tokio::test]
async fn predispatch_store_failure_is_a_known_retryable_rejection() {
    let registry = OperationRegistry::temporary_for_test().unwrap();
    registry.store.clone().shutdown_for_test();
    let id = Uuid::new_v4();

    assert!(matches!(
        registry
            .begin("alice", id, fingerprint(b"never-dispatched"))
            .await
            .unwrap(),
        BeginOperation::Unavailable
    ));
    assert!(matches!(
        registry.status("alice", id).await.unwrap(),
        OperationStatus::Unavailable
    ));

    let mut mutation_response = Response::default();
    apply_registry_unavailable(&mut mutation_response, id).unwrap();
    assert_eq!(mutation_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        mutation_response
            .headers()
            .get(OPERATION_STATE_HEADER)
            .unwrap(),
        "rejected"
    );
    assert_eq!(mutation_response.headers().get("retry-after").unwrap(), "1");
    let body = mutation_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["code"],
        "operation_store_unavailable"
    );

    let mut status_response = Response::default();
    apply_status(&mut status_response, id, OperationStatus::Unavailable).unwrap();
    assert_eq!(status_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        status_response
            .headers()
            .get(OPERATION_STATE_HEADER)
            .unwrap(),
        "unknown"
    );
    assert_eq!(status_response.headers().get("retry-after").unwrap(), "1");
}
