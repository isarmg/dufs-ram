use super::*;
use crate::server::{
    disk_space::DiskSpaceTracker,
    identity::OwnerId,
    rooted_fs::{ReplaceAndSyncOutcome, ReplacementTargetIdentity},
    state_store::StateStore,
    storage::StorageDurability,
};
use futures_util::stream;
use http_body_util::BodyExt as _;
use serde_json::Value;

const TEST_UPLOAD_OWNER: &str = "upload-test-owner";

fn test_owner_id() -> OwnerId {
    OwnerId::persistent(TEST_UPLOAD_OWNER)
}

fn test_upload_record_store(rooted_fs: &RootedFs) -> UploadRecordStore {
    UploadRecordStore::new(
        rooted_fs.clone(),
        StateStore::temporary_for_test(32, 16, UPLOAD_SESSION_TTL).unwrap(),
        UPLOAD_SESSION_TTL,
    )
    .unwrap()
}

fn found_record(lookup: UploadRecordLookup) -> UploadCheckpoint {
    match lookup {
        UploadRecordLookup::Found(checkpoint) => checkpoint,
        UploadRecordLookup::NotSeen => panic!("upload record was not found"),
        UploadRecordLookup::ForeignOwner => panic!("upload record owner did not match"),
    }
}

fn stage_name(target: &str, upload_id: Uuid) -> String {
    get_file_name(&upload_temp_path(Path::new(target), upload_id).unwrap()).to_string()
}

fn stage_path(parent: &Path, target: &str, upload_id: Uuid) -> PathBuf {
    let directory = parent.join(UPLOAD_STAGE_DIRECTORY);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    directory.join(stage_name(target, upload_id))
}

#[test]
fn retained_stage_with_replayed_target_metadata_cannot_become_create_only() {
    let awaiting_overwrite = UploadCheckpoint {
        upload_length: 8,
        durable_offset: 8,
        state: UploadRecordState::AwaitingConfirmation,
        target_revision: Some([7; 32]),
    };
    assert!(!awaiting_stage_is_create_only(&awaiting_overwrite));

    let awaiting_create_only = UploadCheckpoint {
        target_revision: None,
        ..awaiting_overwrite
    };
    assert!(awaiting_stage_is_create_only(&awaiting_create_only));

    let running = UploadCheckpoint {
        state: UploadRecordState::Running,
        ..awaiting_create_only
    };
    assert!(!awaiting_stage_is_create_only(&running));
}

#[tokio::test]
async fn upload_problem_body_matches_the_authoritative_protocol_headers() {
    let cases = [
        (
            StatusCode::REQUEST_TIMEOUT,
            UploadPublicState::Unknown,
            Some(11),
            Some(7),
            ErrorCode::UPLOAD_OUTCOME_UNKNOWN,
            RecoveryAdvice::QueryUpload,
            "query_upload",
        ),
        (
            StatusCode::CONFLICT,
            UploadPublicState::Running,
            Some(11),
            Some(7),
            ErrorCode::UPLOAD_OFFSET_CHANGED,
            RecoveryAdvice::ResumeUpload,
            "resume_upload",
        ),
        (
            StatusCode::CONFLICT,
            UploadPublicState::Rejected,
            Some(11),
            None,
            ErrorCode::UPLOAD_TARGET_CHANGED,
            RecoveryAdvice::RefreshTarget,
            "refresh_target",
        ),
    ];

    for (status, state, upload_length, upload_offset, code, recovery, recovery_wire) in cases {
        let upload_id = Uuid::new_v4();
        let detail = format!("{} upload detail", state.wire_name());
        let mut response = Response::default();
        apply_upload_problem(
            &mut response,
            UploadErrorContext::new(upload_id, state, upload_length, upload_offset),
            status,
            code,
            detail.clone(),
            recovery,
        )
        .unwrap();

        assert_eq!(response.status(), status);
        assert_eq!(
            response.headers()["content-type"],
            "application/problem+json"
        );
        assert_eq!(
            response.headers()["x-dufs-upload-id"],
            upload_id.to_string()
        );
        assert_eq!(
            response.headers()["x-dufs-operation-state"],
            state.wire_name()
        );
        assert_eq!(
            response.headers()["x-dufs-upload-length"],
            upload_length.unwrap().to_string()
        );
        match upload_offset {
            Some(offset) => assert_eq!(
                response.headers()["x-dufs-upload-offset"],
                offset.to_string()
            ),
            None => assert!(!response.headers().contains_key("x-dufs-upload-offset")),
        }

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let problem: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["type"], format!("urn:dufs:problem:{code}"));
        assert_eq!(problem["status"], status.as_u16());
        assert_eq!(problem["detail"], detail);
        assert!(problem.get("message").is_none());
        assert_eq!(problem["code"], code.as_str());
        assert_eq!(problem["recovery"], recovery_wire);
        assert_eq!(problem["upload_id"], upload_id.to_string());
        assert_eq!(problem["upload_state"], state.wire_name());
        assert_eq!(problem["upload_length"], upload_length.unwrap());
        match upload_offset {
            Some(offset) => assert_eq!(problem["upload_offset"], offset),
            None => assert!(problem.get("upload_offset").is_none()),
        }
    }
}

#[tokio::test]
async fn upload_record_store_errors_have_stable_protocol_codes() {
    let cases = [
        (
            UploadRecordStoreError::Conflict,
            StatusCode::CONFLICT,
            "upload_session_conflict",
            "query_upload",
        ),
        (
            UploadRecordStoreError::Full,
            StatusCode::SERVICE_UNAVAILABLE,
            "upload_session_store_full",
            "retry",
        ),
    ];

    for (error, status, code, recovery) in cases {
        let upload_id = Uuid::new_v4();
        let mut response = Response::default();
        assert!(
            apply_upload_record_store_problem(
                &mut response,
                UploadErrorContext::new(upload_id, UploadPublicState::Unknown, Some(9), Some(3),),
                &anyhow::Error::new(error),
            )
            .unwrap()
        );
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["x-dufs-operation-state"], "unknown");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let problem: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], code);
        assert_eq!(problem["recovery"], recovery);
    }

    for dispatch_error in [
        StateStoreDispatchError::QueueFull,
        StateStoreDispatchError::Unavailable,
    ] {
        let upload_id = Uuid::new_v4();
        let mut response = Response::default();
        assert!(
            apply_upload_record_store_problem(
                &mut response,
                UploadErrorContext::new(upload_id, UploadPublicState::Unknown, Some(9), Some(3),),
                &anyhow::Error::new(dispatch_error),
            )
            .unwrap()
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["x-dufs-operation-state"], "unknown");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let problem: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "upload_state_unavailable");
        assert_eq!(problem["recovery"], "query_upload");
    }

    let mut response = Response::default();
    assert!(
        !apply_upload_record_store_problem(
            &mut response,
            UploadErrorContext::new(Uuid::new_v4(), UploadPublicState::Unknown, None, None,),
            &anyhow!("ordinary I/O failure"),
        )
        .unwrap()
    );
}

#[tokio::test]
async fn maintenance_claim_wait_obeys_upload_deadline_and_force_shutdown() {
    let (_changes, mut receiver) = watch::channel(0_u64);
    let force_shutdown = tokio_util::sync::CancellationToken::new();
    let timeout_error = wait_for_maintenance_claim_change(
        &mut receiver,
        Instant::now() + Duration::from_millis(20),
        &force_shutdown,
    )
    .await
    .unwrap_err();
    assert_eq!(timeout_error.kind(), io::ErrorKind::TimedOut);

    let (_changes, mut receiver) = watch::channel(0_u64);
    let force_shutdown = tokio_util::sync::CancellationToken::new();
    force_shutdown.cancel();
    let shutdown_error = wait_for_maintenance_claim_change(
        &mut receiver,
        Instant::now() + Duration::from_secs(1),
        &force_shutdown,
    )
    .await
    .unwrap_err();
    assert_eq!(shutdown_error.kind(), io::ErrorKind::Interrupted);
}

struct FileSyncFailure;

impl StorageDurability for FileSyncFailure {
    async fn sync_file(&self, _file: &fs::File) -> Result<()> {
        anyhow::bail!("injected post-metadata file sync failure")
    }

    async fn replace_and_sync_parents(
        &self,
        _file: &fs::File,
        _source: &Path,
        _destination: &Path,
        _expected_destination: ReplacementTargetIdentity,
    ) -> ReplaceAndSyncOutcome {
        panic!("a failed file sync must not reach rename")
    }
}

#[tokio::test]
async fn upload_body_never_writes_beyond_the_declared_remaining_length() {
    let mut file = fs::File::from_std(tempfile::tempfile().unwrap());
    let tracker = DiskSpaceTracker::new();
    let mut lease = tracker.reserve(&file, 3, 0).unwrap().unwrap();
    let chunks = stream::iter([Ok::<_, anyhow::Error>(Bytes::from_static(b"abcdef"))]);
    let cancellation = tokio_util::sync::CancellationToken::new();

    let error = receive_upload_body(
        chunks,
        &mut file,
        &mut lease,
        UploadTransferOptions {
            remaining: 3,
            minimum_free: 0,
            idle_timeout: Duration::from_secs(30),
            total_deadline: Instant::now() + Duration::from_secs(30),
            force_shutdown: &cancellation,
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(error, UploadTransferError::ExcessBody));
    file.flush().await.unwrap();
    assert_eq!(file.metadata().await.unwrap().len(), 3);
    file.seek(SeekFrom::Start(0)).await.unwrap();
    let mut contents = Vec::new();
    file.read_to_end(&mut contents).await.unwrap();
    assert_eq!(contents, b"abc");
}

#[tokio::test]
async fn upload_body_enforces_idle_and_total_deadlines() {
    let cancellation = tokio_util::sync::CancellationToken::new();

    let mut idle_file = fs::File::from_std(tempfile::tempfile().unwrap());
    let idle_tracker = DiskSpaceTracker::new();
    let mut idle_lease = idle_tracker.reserve(&idle_file, 1, 0).unwrap().unwrap();
    let idle_error = receive_upload_body(
        stream::pending::<Result<Bytes>>(),
        &mut idle_file,
        &mut idle_lease,
        UploadTransferOptions {
            remaining: 1,
            minimum_free: 0,
            idle_timeout: Duration::from_millis(20),
            total_deadline: Instant::now() + Duration::from_secs(1),
            force_shutdown: &cancellation,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(idle_error, UploadTransferError::IdleTimeout));

    let mut total_file = fs::File::from_std(tempfile::tempfile().unwrap());
    let total_tracker = DiskSpaceTracker::new();
    let mut total_lease = total_tracker
        .reserve(&total_file, 1024, 0)
        .unwrap()
        .unwrap();
    let slow_chunks = stream::unfold((), |()| async {
        tokio::time::sleep(Duration::from_millis(5)).await;
        Some((Ok::<_, anyhow::Error>(Bytes::from_static(b"x")), ()))
    });
    let total_error = receive_upload_body(
        slow_chunks,
        &mut total_file,
        &mut total_lease,
        UploadTransferOptions {
            remaining: 1024,
            minimum_free: 0,
            idle_timeout: Duration::from_millis(500),
            total_deadline: Instant::now() + Duration::from_millis(30),
            force_shutdown: &cancellation,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(total_error, UploadTransferError::TotalTimeout));
    assert!(total_file.metadata().await.unwrap().len() < 1024);
}

#[tokio::test]
async fn timed_out_uploads_apply_the_resume_threshold_before_releasing_leases() {
    let owner_id = test_owner_id();
    let small_temp = assert_fs::TempDir::new().unwrap();
    let small_rooted = RootedFs::new(small_temp.path()).unwrap();
    let small_records = test_upload_record_store(&small_rooted);
    let small_id = Uuid::new_v4();
    let small_target = small_temp.path().join("small.bin");
    let small_stage = upload_temp_path(&small_target, small_id).unwrap();
    let mut small_file = create_upload_temp(&small_rooted, &small_stage)
        .await
        .unwrap();
    small_file.write_all(b"small").await.unwrap();
    let mut small_response = Response::default();

    finish_timed_out_upload(
        &small_records,
        small_file,
        &small_target,
        &small_stage,
        owner_id,
        RESUMABLE_UPLOAD_MIN_SIZE,
        small_id,
        &mut small_response,
        UploadTimeout {
            message: "timed out",
            resume_offset: None,
            created_ancestors: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(small_response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        small_response
            .headers()
            .get("x-dufs-operation-state")
            .unwrap(),
        "unknown"
    );
    assert_eq!(
        small_response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    assert_eq!(
        small_response.headers().get("x-dufs-upload-id").unwrap(),
        small_id.to_string().as_str()
    );
    assert!(!small_stage.exists());
    let small_problem: Value = serde_json::from_slice(
        &small_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(small_problem["code"], "upload_outcome_unknown");
    assert_eq!(small_problem["detail"], "timed out");
    assert!(small_problem.get("message").is_none());
    assert_eq!(small_problem["recovery"], "query_upload");
    assert_eq!(small_problem["upload_id"], small_id.to_string());
    assert_eq!(small_problem["upload_state"], "unknown");

    let resumed_temp = assert_fs::TempDir::new().unwrap();
    let resumed_rooted = RootedFs::new(resumed_temp.path()).unwrap();
    let resumed_records = test_upload_record_store(&resumed_rooted);
    let resumed_id = Uuid::new_v4();
    let resumed_target = resumed_temp.path().join("resumed-small.bin");
    let resumed_stage = upload_temp_path(&resumed_target, resumed_id).unwrap();
    let mut resumed_file = create_upload_temp(&resumed_rooted, &resumed_stage)
        .await
        .unwrap();
    resumed_file.write_all(b"small").await.unwrap();
    resumed_records
        .persist_initial_checkpoint(
            &mut resumed_file,
            UploadRecordContext::new(owner_id, resumed_id, &resumed_target, &resumed_stage, 10),
            5,
        )
        .await
        .unwrap();
    resumed_file.write_all(b"more").await.unwrap();
    let mut resumed_response = Response::default();

    finish_timed_out_upload(
        &resumed_records,
        resumed_file,
        &resumed_target,
        &resumed_stage,
        owner_id,
        10,
        resumed_id,
        &mut resumed_response,
        UploadTimeout {
            message: "timed out",
            resume_offset: Some(5),
            created_ancestors: None,
        },
    )
    .await
    .unwrap();

    let resumed_checkpoint = found_record(
        resumed_records
            .lookup(owner_id, resumed_id, &resumed_target, &resumed_stage)
            .await
            .unwrap(),
    );
    assert_eq!(resumed_checkpoint.durable_offset, 9);
    assert!(resumed_stage.exists());
    assert_eq!(
        resumed_response
            .headers()
            .get(UPLOAD_OFFSET_HEADER)
            .unwrap(),
        "9"
    );

    let large_temp = assert_fs::TempDir::new().unwrap();
    let large_rooted = RootedFs::new(large_temp.path()).unwrap();
    let large_records = test_upload_record_store(&large_rooted);
    let large_id = Uuid::new_v4();
    let large_target = large_temp.path().join("large.bin");
    let large_stage = upload_temp_path(&large_target, large_id).unwrap();
    let large_file = create_upload_temp(&large_rooted, &large_stage)
        .await
        .unwrap();
    large_file.set_len(RESUMABLE_UPLOAD_MIN_SIZE).await.unwrap();
    let mut large_response = Response::default();

    finish_timed_out_upload(
        &large_records,
        large_file,
        &large_target,
        &large_stage,
        owner_id,
        RESUMABLE_UPLOAD_MIN_SIZE + 1,
        large_id,
        &mut large_response,
        UploadTimeout {
            message: "timed out",
            resume_offset: None,
            created_ancestors: None,
        },
    )
    .await
    .unwrap();

    let checkpoint = found_record(
        large_records
            .lookup(owner_id, large_id, &large_target, &large_stage)
            .await
            .unwrap(),
    );
    assert_eq!(checkpoint.durable_offset, RESUMABLE_UPLOAD_MIN_SIZE);
    assert_eq!(
        large_response.headers().get(UPLOAD_OFFSET_HEADER).unwrap(),
        RESUMABLE_UPLOAD_MIN_SIZE.to_string().as_str()
    );
    assert_eq!(
        large_response.headers().get("x-dufs-upload-id").unwrap(),
        large_id.to_string().as_str()
    );
}

#[tokio::test]
async fn failed_fresh_upload_cleanup_rolls_back_new_empty_ancestors() {
    let owner_id = test_owner_id();
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let upload_id = Uuid::new_v4();
    let target = temp.path().join("new/deep/file.bin");
    let stage = upload_temp_path(&target, upload_id).unwrap();
    let created = rooted.ensure_parent(&stage).await.unwrap();
    let mut file = create_upload_temp(&rooted, &stage).await.unwrap();
    let records = test_upload_record_store(&rooted);
    records
        .persist_initial_checkpoint(
            &mut file,
            UploadRecordContext::new(owner_id, upload_id, &target, &stage, 1024),
            0,
        )
        .await
        .unwrap();
    drop(file);

    records
        .reset_and_ancestors(owner_id, upload_id, &target, &stage, Some(created))
        .await
        .unwrap();

    assert!(!temp.path().join("new").exists());
    assert!(!stage.exists());
}

#[tokio::test]
async fn resumable_checkpoint_uses_the_actual_nofollow_read_write_open_result() {
    let owner_id = test_owner_id();
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let upload_id = Uuid::new_v4();
    let target = temp.path().join("target.bin");
    let stage = upload_temp_path(&target, upload_id).unwrap();
    let mut file = create_upload_temp(&rooted, &stage).await.unwrap();
    let records = test_upload_record_store(&rooted);
    records
        .persist_initial_checkpoint(
            &mut file,
            UploadRecordContext::new(owner_id, upload_id, &target, &stage, 1024),
            0,
        )
        .await
        .unwrap();
    drop(file);
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o440)).unwrap();

    let actually_openable = rooted.open_write(&stage).await.is_ok();
    assert_eq!(
        records
            .open_resumable_stage(
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 1024),
                0,
                UploadRecordState::Running,
            )
            .await
            .unwrap()
            .is_some(),
        actually_openable,
        "checkpoint recovery must use the same O_RDWR|O_NOFOLLOW capability as PATCH"
    );
}

#[tokio::test]
async fn partial_checkpoint_rejects_a_multiply_linked_stage() {
    let owner_id = test_owner_id();
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let upload_id = Uuid::new_v4();
    let target = temp.path().join("linked.bin");
    let stage = upload_temp_path(&target, upload_id).unwrap();
    let mut file = create_upload_temp(&rooted, &stage).await.unwrap();
    file.write_all(b"part").await.unwrap();
    let records = test_upload_record_store(&rooted);
    records
        .persist_initial_checkpoint(
            &mut file,
            UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8),
            4,
        )
        .await
        .unwrap();
    drop(file);
    std::fs::hard_link(&stage, temp.path().join("stage-alias")).unwrap();

    assert!(matches!(
        records
            .lookup(owner_id, upload_id, &target, &stage)
            .await
            .unwrap(),
        UploadRecordLookup::NotSeen
    ));
}

#[tokio::test]
async fn terminal_and_full_running_records_do_not_become_retryable_not_found() {
    let owner_id = test_owner_id();
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let records = test_upload_record_store(&rooted);

    let committed_id = Uuid::new_v4();
    let committed_target = temp.path().join("committed.bin");
    let committed_stage = upload_temp_path(&committed_target, committed_id).unwrap();
    let mut committed_file = create_upload_temp(&rooted, &committed_stage).await.unwrap();
    committed_file.write_all(b"done").await.unwrap();
    records
        .persist_initial_checkpoint(
            &mut committed_file,
            UploadRecordContext::new(
                owner_id,
                committed_id,
                &committed_target,
                &committed_stage,
                4,
            ),
            4,
        )
        .await
        .unwrap();
    records
        .persist_commit_started(
            &mut committed_file,
            UploadRecordContext::new(
                owner_id,
                committed_id,
                &committed_target,
                &committed_stage,
                4,
            ),
        )
        .await
        .unwrap();
    drop(committed_file);
    std::fs::remove_file(&committed_stage).unwrap();
    records
        .persist_terminal(
            UploadRecordContext::new(
                owner_id,
                committed_id,
                &committed_target,
                &committed_stage,
                4,
            ),
            4,
            UploadRecordState::Committed,
        )
        .await
        .unwrap();
    let committed = found_record(
        records
            .lookup(owner_id, committed_id, &committed_target, &committed_stage)
            .await
            .unwrap(),
    );
    assert_eq!(committed.state, UploadRecordState::Committed);
    assert_eq!(committed.upload_length, 4);
    assert_eq!(committed.durable_offset, 4);
    assert!(matches!(
        records
            .lookup(
                OwnerId::persistent("different-owner"),
                committed_id,
                &committed_target,
                &committed_stage,
            )
            .await
            .unwrap(),
        UploadRecordLookup::NotSeen
    ));

    let uncertain_id = Uuid::new_v4();
    let uncertain_target = temp.path().join("uncertain.bin");
    let uncertain_stage = upload_temp_path(&uncertain_target, uncertain_id).unwrap();
    let mut uncertain_file = create_upload_temp(&rooted, &uncertain_stage).await.unwrap();
    uncertain_file.write_all(b"full").await.unwrap();
    records
        .persist_initial_checkpoint(
            &mut uncertain_file,
            UploadRecordContext::new(
                owner_id,
                uncertain_id,
                &uncertain_target,
                &uncertain_stage,
                4,
            ),
            4,
        )
        .await
        .unwrap();
    drop(uncertain_file);
    std::fs::remove_file(&uncertain_stage).unwrap();
    let uncertain = found_record(
        records
            .lookup(owner_id, uncertain_id, &uncertain_target, &uncertain_stage)
            .await
            .unwrap(),
    );
    assert_eq!(uncertain.state, UploadRecordState::Running);
    assert_eq!(uncertain.durable_offset, 4);

    let altered_id = Uuid::new_v4();
    let altered_target = temp.path().join("altered-full.bin");
    let altered_stage = upload_temp_path(&altered_target, altered_id).unwrap();
    let mut altered_file = create_upload_temp(&rooted, &altered_stage).await.unwrap();
    altered_file.write_all(b"full").await.unwrap();
    records
        .persist_initial_checkpoint(
            &mut altered_file,
            UploadRecordContext::new(owner_id, altered_id, &altered_target, &altered_stage, 4),
            4,
        )
        .await
        .unwrap();
    drop(altered_file);
    std::fs::remove_file(&altered_stage).unwrap();
    std::fs::create_dir(&altered_stage).unwrap();
    let altered = found_record(
        records
            .lookup(owner_id, altered_id, &altered_target, &altered_stage)
            .await
            .unwrap(),
    );
    assert_eq!(altered.state, UploadRecordState::Running);
    assert_eq!(altered.durable_offset, altered.upload_length);

    let partial_id = Uuid::new_v4();
    let partial_target = temp.path().join("partial.bin");
    let partial_stage = upload_temp_path(&partial_target, partial_id).unwrap();
    let mut partial_file = create_upload_temp(&rooted, &partial_stage).await.unwrap();
    partial_file.write_all(b"part").await.unwrap();
    records
        .persist_initial_checkpoint(
            &mut partial_file,
            UploadRecordContext::new(owner_id, partial_id, &partial_target, &partial_stage, 8),
            4,
        )
        .await
        .unwrap();
    drop(partial_file);
    std::fs::remove_file(&partial_stage).unwrap();
    assert!(
        matches!(
            records
                .lookup(owner_id, partial_id, &partial_target, &partial_stage,)
                .await
                .unwrap(),
            UploadRecordLookup::NotSeen
        ),
        "a missing partial stage is not resumable and was never at the publication boundary"
    );
}

#[tokio::test]
async fn post_metadata_sync_failure_is_known_unpublished_and_removes_readonly_session() {
    let owner_id = test_owner_id();
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let target = temp.path().join("target.bin");
    std::fs::write(&target, b"old").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o440)).unwrap();
    let replacement = rooted.replacement_metadata(&target).await.unwrap();
    let upload_id = Uuid::new_v4();
    let stage = upload_temp_path(&target, upload_id).unwrap();
    let mut file = create_upload_temp(&rooted, &stage).await.unwrap();
    file.write_all(b"new").await.unwrap();
    let records = test_upload_record_store(&rooted);
    records
        .persist_initial_checkpoint(
            &mut file,
            UploadRecordContext::new(owner_id, upload_id, &target, &stage, 3),
            0,
        )
        .await
        .unwrap();
    rooted
        .apply_replacement_metadata(
            &file,
            replacement
                .metadata
                .expect("a regular target has preserved metadata"),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&stage).unwrap().permissions().mode() & 0o777,
        0o440
    );

    let outcome = commit_staged_file(
        &FileSyncFailure,
        file,
        &stage,
        &target,
        replacement.identity,
    )
    .await;
    assert!(matches!(outcome, CommitStagedFileOutcome::NotPublished(_)));
    records
        .reset_and_ancestors(owner_id, upload_id, &target, &stage, None)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(!stage.exists());
}

#[test]
fn maintenance_removes_expired_stages_and_trash_but_skips_active_files() {
    let temp = assert_fs::TempDir::new().unwrap();
    let stale = stage_path(temp.path(), "stale.txt", Uuid::new_v4());
    let active_stage = stage_path(temp.path(), "active.txt", Uuid::new_v4());
    let trash = temp.path().join(format!(
        "{DELETE_TRASH_PREFIX}{}{DELETE_TRASH_SUFFIX}",
        Uuid::new_v4()
    ));
    let invalid_session_directory =
        stage_path(temp.path(), "invalid-directory.txt", Uuid::new_v4());
    let ordinary = temp.path().join("ordinary.txt");
    std::fs::write(&stale, "stale").unwrap();
    std::fs::write(&active_stage, "active").unwrap();
    std::fs::create_dir(&trash).unwrap();
    std::fs::write(trash.join("file.txt"), "trash").unwrap();
    std::fs::create_dir(&invalid_session_directory).unwrap();
    std::fs::write(invalid_session_directory.join("keep.txt"), "invalid").unwrap();
    std::fs::write(&ordinary, "ordinary").unwrap();

    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let active = Mutex::new(HashSet::from([rooted_fs
        .entry_key_blocking(&active_stage)
        .unwrap()]));
    let removed = collect_and_remove_stale_internal_files(
        &rooted_fs,
        temp.path(),
        &active,
        SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
        UPLOAD_SESSION_TTL,
        Duration::ZERO,
        None,
    )
    .unwrap();

    assert!(removed.contains(&stale));
    assert!(removed.contains(&trash));
    assert!(!stale.exists());
    assert!(!trash.exists());
    assert!(active_stage.exists());
    assert!(invalid_session_directory.exists());
    assert!(ordinary.exists());
}

#[test]
fn maintenance_batches_resume_to_reach_deep_directories() {
    const DEPTH: usize = 12;
    const ENTRY_BUDGET: usize = 2;
    // The continuation contains the shared root, every ordinary nested
    // directory, and the private upload-stage directory at the leaf.
    const MAX_CONTINUATION_DEPTH: usize = DEPTH + 2;

    let temp = assert_fs::TempDir::new().unwrap();
    let mut directory = temp.path().to_path_buf();
    for depth in 0..DEPTH {
        std::fs::write(directory.join(format!("ordinary-{depth}.txt")), "ordinary").unwrap();
        directory = directory.join(format!("directory-{depth}"));
        std::fs::create_dir(&directory).unwrap();
    }
    let stale = stage_path(&directory, "deep.txt", Uuid::new_v4());
    std::fs::write(&stale, "stale").unwrap();

    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let active = Mutex::new(HashSet::new());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut state = MaintenanceScanState::new(temp.path().to_path_buf(), Duration::ZERO);
    let mut completed = false;
    for _ in 0..128 {
        let (next, _, _, complete, examined) = collect_stale_internal_files_batch(
            &rooted_fs,
            state,
            &active,
            MaintenanceBatchOptions {
                now: SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
                upload_ttl: UPLOAD_SESSION_TTL,
                budget: MaintenanceBudget {
                    max_entries: ENTRY_BUDGET,
                    max_duration: Duration::from_secs(1),
                },
            },
            &shutdown,
            |_| Ok(false),
            |entry| {
                entry.purge_all_blocking().unwrap();
                Ok::<(), Box<TrashEntry>>(())
            },
        );
        assert!(examined <= ENTRY_BUDGET);
        assert!(
            next.directories.len() <= MAX_CONTINUATION_DEPTH,
            "the DFS continuation stack must remain depth-bounded"
        );
        state = next;
        if complete {
            completed = true;
            break;
        }
    }

    assert!(
        completed,
        "bounded maintenance slices must finish a stable tree"
    );
    assert!(!stale.exists(), "the deepest stale entry must not starve");
}

#[test]
fn maintenance_preserves_a_trash_candidate_when_the_queue_is_full() {
    let temp = assert_fs::TempDir::new().unwrap();
    let trash = temp.path().join(format!(
        "{DELETE_TRASH_PREFIX}{}{DELETE_TRASH_SUFFIX}",
        Uuid::new_v4()
    ));
    std::fs::create_dir(&trash).unwrap();
    std::fs::write(trash.join("content.txt"), "trash").unwrap();

    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let active = Mutex::new(HashSet::new());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let state = MaintenanceScanState::new(temp.path().to_path_buf(), Duration::ZERO);
    let mut reject = true;
    let (state, _, _, complete, _) = collect_stale_internal_files_batch(
        &rooted_fs,
        state,
        &active,
        MaintenanceBatchOptions {
            now: SystemTime::now(),
            upload_ttl: UPLOAD_SESSION_TTL,
            budget: MaintenanceBudget {
                max_entries: 16,
                max_duration: Duration::from_secs(1),
            },
        },
        &shutdown,
        |_| Ok(false),
        |entry| {
            if reject {
                reject = false;
                Err(Box::new(entry))
            } else {
                entry.purge_all_blocking().unwrap();
                Ok(())
            }
        },
    );
    assert!(!complete);
    assert!(state.pending_purge.is_some());
    assert!(trash.exists());

    let (state, _, _, complete, _) = collect_stale_internal_files_batch(
        &rooted_fs,
        state,
        &active,
        MaintenanceBatchOptions {
            now: SystemTime::now(),
            upload_ttl: UPLOAD_SESSION_TTL,
            budget: MaintenanceBudget {
                max_entries: 16,
                max_duration: Duration::from_secs(1),
            },
        },
        &shutdown,
        |_| Ok(false),
        |entry| {
            entry.purge_all_blocking().unwrap();
            Ok::<(), Box<TrashEntry>>(())
        },
    );
    assert!(complete);
    assert!(state.pending_purge.is_none());
    assert!(!trash.exists());
}

#[test]
fn saturated_purge_queue_does_not_starve_other_maintenance_entries() {
    let temp = assert_fs::TempDir::new().unwrap();
    let trash = temp.path().join(format!(
        "{DELETE_TRASH_PREFIX}{}{DELETE_TRASH_SUFFIX}",
        Uuid::new_v4()
    ));
    std::fs::create_dir(&trash).unwrap();
    std::fs::write(trash.join("content.txt"), "trash").unwrap();
    let stale = stage_path(temp.path(), "stale-after-trash.txt", Uuid::new_v4());
    std::fs::write(&stale, "stale").unwrap();

    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let active = Mutex::new(HashSet::new());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut state = MaintenanceScanState::new(temp.path().to_path_buf(), Duration::ZERO);
    let mut complete = false;
    for _ in 0..8 {
        let (next, _, _, batch_complete, _) = collect_stale_internal_files_batch(
            &rooted_fs,
            state,
            &active,
            MaintenanceBatchOptions {
                now: SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
                upload_ttl: UPLOAD_SESSION_TTL,
                budget: MaintenanceBudget {
                    max_entries: 16,
                    max_duration: Duration::from_secs(1),
                },
            },
            &shutdown,
            |_| Ok(false),
            |entry| Err(Box::new(entry)),
        );
        state = next;
        if batch_complete {
            complete = true;
            break;
        }
    }

    assert!(
        complete,
        "a saturated purge queue must not pin the maintenance cursor"
    );
    assert!(
        trash.exists(),
        "queue rejection must leave trash recoverable"
    );
    assert!(
        !stale.exists(),
        "unrelated stale upload files must continue to be cleaned"
    );
}

#[test]
fn maintenance_time_budget_can_pause_before_examining_entries() {
    let temp = assert_fs::TempDir::new().unwrap();
    std::fs::write(temp.path().join("ordinary.txt"), "ordinary").unwrap();
    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let state = MaintenanceScanState::new(temp.path().to_path_buf(), Duration::ZERO);

    let (state, _, _, complete, examined) = collect_stale_internal_files_batch(
        &rooted_fs,
        state,
        &Mutex::new(HashSet::new()),
        MaintenanceBatchOptions {
            now: SystemTime::now(),
            upload_ttl: UPLOAD_SESSION_TTL,
            budget: MaintenanceBudget {
                max_entries: 16,
                max_duration: Duration::ZERO,
            },
        },
        &shutdown,
        |_| Ok(false),
        |_| unreachable!("a zero-time batch cannot discover trash"),
    );

    assert!(!complete);
    assert_eq!(examined, 0);
    assert_eq!(state.directories.len(), 1);
    assert_eq!(state.directories[0].cursor, DirectoryCursor::default());
}

#[test]
fn maintenance_rechecks_the_live_lease_set_before_deleting() {
    let temp = assert_fs::TempDir::new().unwrap();
    let stage = stage_path(temp.path(), "race.txt", Uuid::new_v4());
    std::fs::write(&stage, "stale").unwrap();
    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let stage_key = rooted_fs.entry_key_blocking(&stage).unwrap();
    let active = Arc::new(Mutex::new(HashSet::new()));
    let mut registration = active.lock().unwrap();

    let cleaner = {
        let root = temp.path().to_path_buf();
        let active = active.clone();
        std::thread::spawn(move || {
            collect_and_remove_stale_internal_files(
                &rooted_fs,
                &root,
                &active,
                SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
                UPLOAD_SESSION_TTL,
                Duration::ZERO,
                None,
            )
            .unwrap()
        })
    };
    std::thread::sleep(Duration::from_millis(20));
    registration.insert(stage_key);
    drop(registration);

    let removed = cleaner.join().unwrap();
    assert!(!removed.contains(&stage));
    assert!(stage.exists());
}

#[test]
fn maintenance_recognizes_active_uploads_through_root_internal_symlink_aliases() {
    use std::os::unix::fs::symlink;

    let temp = assert_fs::TempDir::new().unwrap();
    let target = temp.path().join("target");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&target).unwrap();
    symlink("target", &alias).unwrap();

    let aliased_stage = stage_path(&alias, "aliased.txt", Uuid::new_v4());
    let stage_name = aliased_stage.file_name().unwrap().to_owned();
    std::fs::write(&aliased_stage, "active-stage").unwrap();

    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let active = Mutex::new(HashSet::from([rooted_fs
        .entry_key_blocking(&aliased_stage)
        .unwrap()]));
    let removed = collect_and_remove_stale_internal_files(
        &rooted_fs,
        temp.path(),
        &active,
        SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
        UPLOAD_SESSION_TTL,
        Duration::ZERO,
        None,
    )
    .unwrap();

    assert!(removed.is_empty());
    assert_eq!(
        std::fs::read_to_string(target.join(UPLOAD_STAGE_DIRECTORY).join(&stage_name),).unwrap(),
        "active-stage"
    );
}

#[test]
fn invalid_prefixed_names_are_not_internal_or_removed() {
    let temp = assert_fs::TempDir::new().unwrap();
    let upload_id = Uuid::new_v4();
    let canonical_stage = stage_name("target.txt", upload_id);
    let uppercase_target_tag_stage = format!(
        "{UPLOAD_TEMP_PREFIX}{}-{upload_id}{UPLOAD_TEMP_SUFFIX}",
        "A".repeat(64)
    );
    let invalid_names = [
        ".dufs-upload-not-a-stage.part".to_string(),
        ".dufs-upload-delete-old.trash".to_string(),
        format!("{canonical_stage}.extra"),
        format!(
            "{DELETE_TRASH_PREFIX}{}{DELETE_TRASH_SUFFIX}.extra",
            Uuid::new_v4()
        ),
        uppercase_target_tag_stage,
    ];

    for name in &invalid_names {
        assert!(
            !crate::server::internal_names::is_internal_name(name),
            "{name}"
        );
        std::fs::write(temp.path().join(name), "ordinary").unwrap();
    }

    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let removed = collect_and_remove_stale_internal_files(
        &rooted_fs,
        temp.path(),
        &Mutex::new(HashSet::new()),
        SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
        UPLOAD_SESSION_TTL,
        Duration::ZERO,
        None,
    )
    .unwrap();

    assert!(removed.is_empty());
    for name in &invalid_names {
        assert_eq!(
            std::fs::read_to_string(temp.path().join(name)).unwrap(),
            "ordinary"
        );
    }
}

#[test]
fn maintenance_stays_on_the_opened_root_after_path_replacement() {
    let temp = assert_fs::TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let moved_root = root.with_extension("opened-root");
    let stage = PathBuf::from(UPLOAD_STAGE_DIRECTORY).join(stage_name("stale.txt", Uuid::new_v4()));
    std::fs::create_dir(root.join(UPLOAD_STAGE_DIRECTORY)).unwrap();
    std::fs::set_permissions(
        root.join(UPLOAD_STAGE_DIRECTORY),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(root.join(&stage), "original").unwrap();
    let rooted_fs = RootedFs::new(&root).unwrap();

    std::fs::rename(&root, &moved_root).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join(UPLOAD_STAGE_DIRECTORY)).unwrap();
    std::fs::set_permissions(
        root.join(UPLOAD_STAGE_DIRECTORY),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(root.join(&stage), "replacement").unwrap();

    let result = collect_and_remove_stale_internal_files(
        &rooted_fs,
        &root,
        &Mutex::new(HashSet::new()),
        SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
        UPLOAD_SESSION_TTL,
        Duration::ZERO,
        None,
    );

    assert!(result.is_ok());
    assert!(!moved_root.join(&stage).exists());
    assert_eq!(
        std::fs::read_to_string(root.join(&stage)).unwrap(),
        "replacement"
    );
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::rename(&moved_root, &root).unwrap();
}
