use super::*;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::Command as ProcessCommand;
use tempfile::tempdir;

const CAPACITY: usize = 8;
const PER_OWNER: usize = 4;
const TTL: Duration = Duration::from_secs(60);
type SchemaRow = (String, String, String);
type DatabaseSchemaSnapshot = (String, Vec<SchemaRow>);
const HOT_ROLLBACK_FIXTURE_PATH: &str = "DUFS_TEST_HOT_ROLLBACK_FIXTURE_PATH";
const HOT_ROLLBACK_FIXTURE_KIND: &str = "DUFS_TEST_HOT_ROLLBACK_FIXTURE_KIND";
const HOT_ROLLBACK_ORDINARY_OPERATION: &str = "ordinary-operation";
const HOT_ROLLBACK_TRUSTED_MAIN: &str = "trusted-main";
const HOT_ROLLBACK_TRUSTED_ROOT_DEVICE: u64 = 769;
const HOT_ROLLBACK_TRUSTED_ROOT_INODE: u64 = 773;

#[derive(Debug, Eq, PartialEq)]
struct FileSnapshot {
    bytes: Vec<u8>,
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    uid: u32,
    gid: u32,
}

#[test]
fn sqlite_hot_rollback_crash_fixture_helper() -> Result<()> {
    let Some(path) = std::env::var_os(HOT_ROLLBACK_FIXTURE_PATH) else {
        return Ok(());
    };
    let connection = Connection::open(PathBuf::from(path))?;
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    ensure!(mode.eq_ignore_ascii_case("delete"));
    connection.pragma_update(None, "synchronous", "FULL")?;
    match std::env::var(HOT_ROLLBACK_FIXTURE_KIND).as_deref() {
        Ok(HOT_ROLLBACK_ORDINARY_OPERATION) => connection.execute_batch(
            "BEGIN IMMEDIATE;
             INSERT INTO operations(
                 owner_digest, operation_id, fingerprint, lease_token, state,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 zeroblob(32), zeroblob(16), zeroblob(32), zeroblob(16), 0,
                 1, 1
             );",
        )?,
        Ok(HOT_ROLLBACK_TRUSTED_MAIN) => {
            connection.execute_batch("BEGIN IMMEDIATE;")?;
            connection.execute(
                "UPDATE store_meta
                    SET value = CASE key
                        WHEN 'root-device-be' THEN ?1
                        ELSE ?2
                    END
                  WHERE key IN ('root-device-be', 'root-inode-be')",
                params![
                    HOT_ROLLBACK_TRUSTED_ROOT_DEVICE.to_be_bytes().as_slice(),
                    HOT_ROLLBACK_TRUSTED_ROOT_INODE.to_be_bytes().as_slice()
                ],
            )?;
        }
        _ => connection.execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE store_meta
                SET value = X'FFFFFFFFFFFFFFFF'
              WHERE key = 'root-device-be';",
        )?,
    }
    connection.cache_flush()?;
    // Exit without Rust or SQLite destructors so the rollback journal remains
    // genuinely hot and the flushed main page still needs recovery.
    std::process::exit(86);
}

fn key(owner: u8, id: u8) -> OperationKey {
    OperationKey {
        owner: [owner; 32],
        id: [id; 16],
    }
}

fn fingerprint(value: u8) -> [u8; 32] {
    [value; 32]
}

fn success(status: u16) -> StoredOutcome {
    StoredOutcome {
        status,
        state: StoredTerminalState::Succeeded,
        code: None,
    }
}

fn root(device: u64, inode: u64) -> RootIdentity {
    RootIdentity::new(device, inode)
}

fn upload(owner: u8, id: u8, offset: u64) -> StoredUploadSession {
    StoredUploadSession {
        key: UploadSessionKey {
            owner: [owner; 32],
            id: [id; 16],
        },
        target_path: PathBuf::from(format!("targets/{owner}-{id}")),
        stage_path: PathBuf::from(format!("staging/{owner}-{id}")),
        upload_length: 10,
        durable_offset: offset,
        state: StoredUploadState::Running,
        stage_identity: None,
        target_revision: None,
    }
}

fn purge(owner: u8, id: u8) -> StoredPurgeJob {
    StoredPurgeJob {
        key: PurgeJobKey {
            owner: [owner; 32],
            id: [id; 16],
        },
        target_path: PathBuf::from(format!("targets/{owner}-{id}")),
        trash_path: PathBuf::from(format!("trash/{owner}-{id}")),
        source_identity: StoredFileIdentity {
            device: u64::from(owner),
            inode: u64::from(id),
        },
        trash_revision: None,
        is_directory: false,
        state: StoredPurgeState::Prepared,
        attempts: 0,
    }
}

fn repository_limits(
    upload_capacity: usize,
    upload_per_owner: usize,
    purge_capacity: usize,
    purge_per_owner: usize,
) -> RepositoryLimits {
    RepositoryLimits {
        upload_capacity,
        upload_per_owner,
        purge_capacity,
        purge_per_owner,
    }
}

fn temporary_with_repository_limits(limits: RepositoryLimits) -> Result<StateStore> {
    StateStore::temporary_with_limits_for_test(
        CAPACITY,
        PER_OWNER,
        TTL,
        COMMAND_QUEUE_CAPACITY,
        limits,
    )
}

fn database_schema_snapshot(path: &Path) -> Result<DatabaseSchemaSnapshot> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let journal_mode = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let mut statement = connection.prepare(
        "SELECT type, name, COALESCE(sql, '') FROM sqlite_schema
          WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
    )?;
    let schema = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((journal_mode, schema))
}

fn state_database_files_snapshot(path: &Path) -> Result<Vec<Option<FileSnapshot>>> {
    std::iter::once("")
        .chain(database::SQLITE_SIDECAR_SUFFIXES)
        .map(|suffix| {
            let mut snapshot_path = path.as_os_str().to_os_string();
            snapshot_path.push(suffix);
            let snapshot_path = PathBuf::from(snapshot_path);
            match fs::symlink_metadata(&snapshot_path) {
                Ok(metadata) => Ok(Some(FileSnapshot {
                    bytes: fs::read(&snapshot_path)?,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    mode: metadata.mode(),
                    links: metadata.nlink(),
                    uid: metadata.uid(),
                    gid: metadata.gid(),
                })),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .collect()
}

#[tokio::test]
async fn operation_lifecycle_replays_exact_outcome() -> Result<()> {
    let store = StateStore::temporary_for_test(CAPACITY, PER_OWNER, TTL)?;
    let key = key(1, 1);
    let lease = match store.begin_operation(key, fingerprint(1)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    assert_eq!(
        store.begin_operation(key, fingerprint(1)).await?,
        StoreBegin::Running
    );
    assert_eq!(
        store.begin_operation(key, fingerprint(2)).await?,
        StoreBegin::Conflict
    );
    assert!(store.mark_operation_commit_started(key, lease).await?);
    assert!(!store.mark_operation_commit_started(key, lease).await?);
    let outcome = success(201);
    assert!(
        store
            .complete_operation(key, lease, outcome.clone())
            .await?
    );
    assert_eq!(
        store.operation_status(key).await?,
        StoreStatus::Completed(outcome.clone())
    );
    assert_eq!(
        store.begin_operation(key, fingerprint(1)).await?,
        StoreBegin::Replay(outcome)
    );
    assert!(store.is_healthy());
    Ok(())
}

#[tokio::test]
async fn readiness_probe_checks_the_live_read_and_write_paths() -> Result<()> {
    let store = StateStore::temporary_for_test(CAPACITY, PER_OWNER, TTL)?;

    store.probe_readiness().await?;
    store.set_query_only(true).await?;
    let error = store
        .probe_readiness()
        .await
        .expect_err("query-only mode must fail the readiness write probe");
    assert!(
        format!("{error:#}").contains("readiness"),
        "unexpected readiness error: {error:#}"
    );
    assert!(store.is_healthy());
    store.set_query_only(false).await?;
    store.probe_readiness().await?;
    assert!(store.is_healthy());
    assert_eq!(
        store.operation_status(key(7, 7)).await?,
        StoreStatus::NotFound
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_command_error_does_not_stop_the_actor() -> Result<()> {
    let store = StateStore::temporary_for_test(CAPACITY, PER_OWNER, TTL)?;

    let error = store
        .inject_sql_error()
        .await
        .expect_err("the injected SQL statement must fail");
    assert!(
        format!("{error:#}").contains("__dufs_missing_test_table"),
        "unexpected injected error: {error:#}"
    );

    assert!(store.is_healthy());
    store.probe_readiness().await?;
    assert!(matches!(
        store.begin_operation(key(8, 8), fingerprint(8)).await?,
        StoreBegin::Started { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn cancelled_begin_delivery_does_not_leave_a_reservation() -> Result<()> {
    let store = StateStore::temporary_for_test(CAPACITY, PER_OWNER, TTL)?;

    // Cancellation already visible before execution skips the reservation.
    let cancelled_before_reply = key(1, 1);
    let (reply, receiver) = oneshot::channel();
    drop(receiver);
    store.send(Command::Begin {
        key: cancelled_before_reply,
        fingerprint: fingerprint(1),
        reply,
    })?;
    assert_eq!(
        store.operation_status(cancelled_before_reply).await?,
        StoreStatus::NotFound
    );

    // Cancellation after send succeeds but before the future receives its
    // value drops BeginEnvelope and exercises its FIFO cleanup fallback.
    let cancelled_after_reply = key(1, 2);
    let (reply, receiver) = oneshot::channel();
    store.send(Command::Begin {
        key: cancelled_after_reply,
        fingerprint: fingerprint(2),
        reply,
    })?;
    let _ = store.operation_status(key(9, 9)).await?;
    drop(receiver);
    assert_eq!(
        store.operation_status(cancelled_after_reply).await?,
        StoreStatus::NotFound
    );
    Ok(())
}

#[tokio::test]
async fn queued_cancelled_status_and_begin_are_skipped_before_execution() -> Result<()> {
    let store = StateStore::temporary_for_test(CAPACITY, PER_OWNER, TTL)?;
    let release = store.block_actor_for_test()?;

    for id in 0..64 {
        let (reply, receiver) = oneshot::channel();
        drop(receiver);
        store.send(Command::Status {
            key: key(2, id),
            reply,
        })?;
    }
    let cancelled_begin = key(3, 1);
    let (reply, receiver) = oneshot::channel();
    drop(receiver);
    store.send(Command::Begin {
        key: cancelled_begin,
        fingerprint: fingerprint(3),
        reply,
    })?;

    release.send(())?;
    assert_eq!(
        store.inspect_actor_execution_counts().await?,
        ActorExecutionCounts::default(),
        "cancelled result-only commands reached SQLite"
    );
    assert_eq!(
        store.operation_status(cancelled_begin).await?,
        StoreStatus::NotFound
    );
    assert_eq!(
        store.inspect_actor_execution_counts().await?,
        ActorExecutionCounts {
            status: 1,
            ..ActorExecutionCounts::default()
        },
        "the execution counter did not distinguish live from cancelled work"
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_mutating_command_still_executes() -> Result<()> {
    let store = StateStore::temporary_for_test(CAPACITY, PER_OWNER, TTL)?;
    let operation = key(4, 1);
    let lease = match store.begin_operation(operation, fingerprint(4)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    assert_eq!(
        store.inspect_actor_execution_counts().await?,
        ActorExecutionCounts {
            begin: 1,
            ..ActorExecutionCounts::default()
        }
    );

    let release = store.block_actor_for_test()?;
    let outcome = success(204);
    let (reply, receiver) = oneshot::channel();
    drop(receiver);
    store.send(Command::Complete {
        key: operation,
        lease,
        outcome: outcome.clone(),
        reply,
    })?;
    release.send(())?;

    assert_eq!(
        store.inspect_actor_execution_counts().await?,
        ActorExecutionCounts {
            begin: 1,
            complete: 1,
            ..ActorExecutionCounts::default()
        },
        "a mutating command was skipped because its reply was cancelled"
    );
    assert_eq!(
        store.operation_status(operation).await?,
        StoreStatus::Completed(outcome)
    );
    Ok(())
}

#[tokio::test]
async fn abandon_removes_reserved_and_marks_commit_unknown() -> Result<()> {
    let store = StateStore::temporary_for_test(CAPACITY, PER_OWNER, TTL)?;
    let reserved = key(1, 1);
    let reserved_lease = match store.begin_operation(reserved, fingerprint(1)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    store.abandon_operation(reserved, reserved_lease);
    assert_eq!(
        store.operation_status(reserved).await?,
        StoreStatus::NotFound
    );

    let committing = key(1, 2);
    let committing_lease = match store.begin_operation(committing, fingerprint(2)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    assert!(
        store
            .mark_operation_commit_started(committing, committing_lease)
            .await?
    );
    store.abandon_operation(committing, committing_lease);
    assert_eq!(
        store.operation_status(committing).await?,
        StoreStatus::Completed(StoredOutcome::uncertain())
    );
    Ok(())
}

#[tokio::test]
async fn abandon_sql_error_is_deferred_without_stopping_the_actor() -> Result<()> {
    let store = StateStore::temporary_for_test(CAPACITY, PER_OWNER, TTL)?;
    let operation = key(1, 7);
    let lease = match store.begin_operation(operation, fingerprint(7)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };

    store.set_query_only(true).await?;
    store.abandon_operation(operation, lease);
    store.set_query_only(false).await?;

    assert!(store.is_healthy());
    assert_eq!(
        store.operation_status(operation).await?,
        StoreStatus::NotFound
    );
    store.probe_readiness().await?;
    Ok(())
}

#[tokio::test]
async fn enforces_global_and_per_owner_capacity() -> Result<()> {
    let store = StateStore::temporary_for_test(2, 1, TTL)?;
    assert!(matches!(
        store.begin_operation(key(1, 1), fingerprint(1)).await?,
        StoreBegin::Started { .. }
    ));
    assert_eq!(
        store.begin_operation(key(1, 2), fingerprint(2)).await?,
        StoreBegin::Full
    );
    assert!(matches!(
        store.begin_operation(key(2, 2), fingerprint(2)).await?,
        StoreBegin::Started { .. }
    ));
    assert_eq!(
        store.begin_operation(key(3, 3), fingerprint(3)).await?,
        StoreBegin::Full
    );
    Ok(())
}

#[tokio::test]
async fn completed_operations_hold_capacity_before_expiration() -> Result<()> {
    let store = StateStore::temporary_for_test(1, 1, TTL)?;
    let first = key(1, 1);
    let lease = match store.begin_operation(first, fingerprint(1)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    assert!(store.complete_operation(first, lease, success(204)).await?);
    assert_eq!(
        store.begin_operation(key(2, 2), fingerprint(2)).await?,
        StoreBegin::Full
    );
    Ok(())
}

#[tokio::test]
async fn expiration_releases_capacity() -> Result<()> {
    let store = StateStore::temporary_for_test(1, 1, Duration::ZERO)?;
    let first = key(1, 1);
    let lease = match store.begin_operation(first, fingerprint(1)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    assert!(store.complete_operation(first, lease, success(204)).await?);
    assert!(matches!(
        store.begin_operation(key(2, 2), fingerprint(2)).await?,
        StoreBegin::Started { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn restart_recovers_nonterminal_operations() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(10, 20);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;

    let reserved = key(1, 1);
    let _reserved_lease = match store.begin_operation(reserved, fingerprint(1)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    let committing = key(1, 2);
    let committing_lease = match store.begin_operation(committing, fingerprint(2)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    assert!(
        store
            .mark_operation_commit_started(committing, committing_lease)
            .await?
    );
    store.shutdown_for_test();

    let reopened = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    assert_eq!(
        reopened.operation_status(reserved).await?,
        StoreStatus::NotFound
    );
    assert_eq!(
        reopened.operation_status(committing).await?,
        StoreStatus::Completed(StoredOutcome::uncertain())
    );
    Ok(())
}

#[tokio::test]
async fn completed_outcome_survives_restart() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(u64::MAX, u64::MAX - 1);
    let operation = key(3, 4);
    let outcome = success(204);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    let lease = match store.begin_operation(operation, fingerprint(9)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };
    assert!(
        store
            .complete_operation(operation, lease, outcome.clone())
            .await?
    );
    store.shutdown_for_test();

    let reopened = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    assert_eq!(
        reopened.begin_operation(operation, fingerprint(9)).await?,
        StoreBegin::Replay(outcome)
    );
    Ok(())
}

#[tokio::test]
async fn upload_sessions_enforce_bindings_transitions_and_capacity() -> Result<()> {
    let store = temporary_with_repository_limits(repository_limits(2, 1, 8, 4))?;
    let mut first = upload(1, 1, 0);
    assert_eq!(
        store.save_upload_session(first.clone(), TTL).await?,
        StoreUploadSession::Inserted
    );
    assert_eq!(
        store.save_upload_session(first.clone(), TTL).await?,
        StoreUploadSession::Unchanged
    );

    let mut wrong_binding = first.clone();
    wrong_binding.target_path = PathBuf::from("targets/different");
    assert_eq!(
        store.save_upload_session(wrong_binding, TTL).await?,
        StoreUploadSession::Conflict
    );

    first.durable_offset = 4;
    first.stage_identity = Some(StoredFileIdentity {
        device: 11,
        inode: 12,
    });
    assert_eq!(
        store.save_upload_session(first.clone(), TTL).await?,
        StoreUploadSession::Updated
    );
    assert_eq!(store.upload_session(first.key).await?, Some(first.clone()));

    let mut backwards = first.clone();
    backwards.durable_offset = 3;
    assert_eq!(
        store.save_upload_session(backwards, TTL).await?,
        StoreUploadSession::Conflict
    );
    assert_eq!(
        store.save_upload_session(upload(1, 2, 0), TTL).await?,
        StoreUploadSession::Full
    );
    assert_eq!(
        store.save_upload_session(upload(2, 2, 0), TTL).await?,
        StoreUploadSession::Inserted
    );
    assert_eq!(
        store.save_upload_session(upload(3, 3, 0), TTL).await?,
        StoreUploadSession::Full
    );

    first.durable_offset = first.upload_length;
    first.state = StoredUploadState::CommitStarted;
    assert_eq!(
        store
            .save_upload_session(first.clone(), Duration::ZERO)
            .await?,
        StoreUploadSession::Updated
    );
    assert!(
        store
            .expired_upload_sessions_page(None, 8)
            .await?
            .is_empty(),
        "an in-flight publication must not be expired while the process is alive"
    );
    first.state = StoredUploadState::Committed;
    assert_eq!(
        store
            .save_upload_session(first.clone(), Duration::ZERO)
            .await?,
        StoreUploadSession::Updated
    );
    let expired = store.expired_upload_sessions_page(None, 8).await?;
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].session, first.clone());

    // Insertion purges expired terminal sessions before applying quotas.
    assert_eq!(
        store.save_upload_session(upload(3, 3, 0), TTL).await?,
        StoreUploadSession::Inserted
    );
    assert_eq!(store.upload_session(first.key).await?, None);
    let replacement_key = UploadSessionKey {
        owner: [3; 32],
        id: [3; 16],
    };
    let replacement = store
        .upload_session(replacement_key)
        .await?
        .expect("the capacity replacement session is missing");
    assert!(
        store
            .remove_upload_session_if_matches(replacement.clone())
            .await?
    );
    assert!(!store.remove_upload_session_if_matches(replacement).await?);
    Ok(())
}

#[tokio::test]
async fn startup_upload_snapshot_is_bounded_and_keyset_paginated() -> Result<()> {
    let store = temporary_with_repository_limits(repository_limits(32, 32, 8, 4))?;
    let mut expected = Vec::new();
    for owner in 1..=17 {
        let session = upload(owner, 1, 0);
        assert_eq!(
            store.save_upload_session(session.clone(), TTL).await?,
            StoreUploadSession::Inserted
        );
        expected.push(session);
    }

    let mut after = None;
    let mut actual = Vec::new();
    loop {
        let page = store.upload_sessions_page_blocking(after, 5)?;
        assert!(page.len() <= 5);
        if page.is_empty() {
            break;
        }
        after = page.last().map(|session| session.key);
        actual.extend(page);
    }
    assert_eq!(actual, expected);
    assert!(store.upload_sessions_page_blocking(None, 65).is_err());
    Ok(())
}

#[tokio::test]
async fn conditional_upload_removal_preserves_newer_and_refreshed_snapshots() -> Result<()> {
    let store = temporary_with_repository_limits(repository_limits(8, 4, 8, 4))?;
    let original = upload(9, 9, 0);
    assert_eq!(
        store.save_upload_session(original.clone(), TTL).await?,
        StoreUploadSession::Inserted
    );

    let mut newer = original.clone();
    newer.durable_offset = 4;
    newer.stage_identity = Some(StoredFileIdentity {
        device: 41,
        inode: 42,
    });
    assert_eq!(
        store.save_upload_session(newer.clone(), TTL).await?,
        StoreUploadSession::Updated
    );
    assert!(
        !store.remove_upload_session_if_matches(original).await?,
        "a stale maintenance snapshot removed a newer upload row"
    );
    assert_eq!(store.upload_session(newer.key).await?, Some(newer.clone()));
    assert!(
        store
            .remove_upload_session_if_matches(newer.clone())
            .await?
    );
    assert_eq!(store.upload_session(newer.key).await?, None);

    let refreshed = upload(10, 10, 0);
    assert_eq!(
        store
            .save_upload_session(refreshed.clone(), Duration::ZERO)
            .await?,
        StoreUploadSession::Inserted
    );
    let expired_snapshot = store
        .expired_upload_sessions_page(None, 8)
        .await?
        .into_iter()
        .find(|snapshot| snapshot.session.key == refreshed.key)
        .expect("the zero-TTL upload was not visible to maintenance");
    assert_eq!(expired_snapshot.session, refreshed);
    assert!(
        store
            .expired_upload_session_matches(expired_snapshot.clone())
            .await?
    );
    assert_eq!(
        store.save_upload_session(refreshed.clone(), TTL).await?,
        StoreUploadSession::Unchanged
    );
    assert!(
        !store
            .expired_upload_session_matches(expired_snapshot.clone())
            .await?,
        "an old expiry snapshot still matched after its TTL was refreshed"
    );
    assert!(
        !store
            .remove_expired_upload_session_if_matches(expired_snapshot)
            .await?,
        "an old expiry scan removed the same business snapshot after its TTL was refreshed"
    );
    assert_eq!(
        store.upload_session(refreshed.key).await?,
        Some(refreshed.clone())
    );
    assert!(store.remove_upload_session_if_matches(refreshed).await?);
    Ok(())
}

#[tokio::test]
async fn opportunistic_terminal_purge_preserves_rejected_cleanup_identity() -> Result<()> {
    let store = temporary_with_repository_limits(repository_limits(8, 4, 8, 4))?;
    let mut rejected = upload(11, 11, 10);
    rejected.state = StoredUploadState::Rejected;
    rejected.stage_identity = Some(StoredFileIdentity {
        device: 51,
        inode: 52,
    });
    assert_eq!(
        store
            .save_upload_session(rejected.clone(), Duration::ZERO)
            .await?,
        StoreUploadSession::Inserted
    );

    let mut identity_free = upload(12, 12, 10);
    identity_free.state = StoredUploadState::Rejected;
    assert_eq!(
        store
            .save_upload_session(identity_free.clone(), Duration::ZERO)
            .await?,
        StoreUploadSession::Inserted
    );

    let unrelated = upload(13, 13, 0);
    assert_eq!(
        store.save_upload_session(unrelated, TTL).await?,
        StoreUploadSession::Inserted
    );
    assert_eq!(
        store.upload_session(rejected.key).await?,
        Some(rejected.clone()),
        "an unrelated save discarded the identity needed to finish rejected-stage cleanup"
    );
    assert_eq!(
        store.upload_session(identity_free.key).await?,
        None,
        "an identity-free rejected session did not release expired capacity"
    );
    assert!(store.remove_upload_session_if_matches(rejected).await?);
    Ok(())
}

#[tokio::test]
async fn upload_rejection_is_update_only_bound_and_does_not_refresh_terminal_ttl() -> Result<()> {
    let store = temporary_with_repository_limits(repository_limits(8, 4, 8, 4))?;
    let mut awaiting = upload(11, 11, 0);
    assert_eq!(
        store
            .reject_upload_session(
                awaiting.key,
                awaiting.target_path.clone(),
                awaiting.stage_path.clone(),
                TTL,
            )
            .await?,
        RejectUploadSession::NotFound
    );
    assert_eq!(store.upload_session(awaiting.key).await?, None);

    awaiting.durable_offset = awaiting.upload_length;
    awaiting.state = StoredUploadState::AwaitingConfirmation;
    awaiting.stage_identity = Some(StoredFileIdentity {
        device: 51,
        inode: 52,
    });
    awaiting.target_revision = Some([53; 32]);
    assert_eq!(
        store.save_upload_session(awaiting.clone(), TTL).await?,
        StoreUploadSession::Inserted
    );
    assert_eq!(
        store
            .reject_upload_session(
                awaiting.key,
                PathBuf::from("targets/other"),
                awaiting.stage_path.clone(),
                TTL,
            )
            .await?,
        RejectUploadSession::BindingConflict
    );
    assert_eq!(
        store.upload_session(awaiting.key).await?,
        Some(awaiting.clone())
    );

    let running = upload(12, 12, 0);
    assert_eq!(
        store.save_upload_session(running.clone(), TTL).await?,
        StoreUploadSession::Inserted
    );
    assert_eq!(
        store
            .reject_upload_session(
                running.key,
                running.target_path.clone(),
                running.stage_path.clone(),
                TTL,
            )
            .await?,
        RejectUploadSession::StateConflict(running.clone())
    );
    assert_eq!(store.upload_session(running.key).await?, Some(running));

    store.set_query_only(true).await?;
    assert!(
        store
            .reject_upload_session(
                awaiting.key,
                awaiting.target_path.clone(),
                awaiting.stage_path.clone(),
                TTL,
            )
            .await
            .is_err(),
        "a read-only state database accepted an Awaiting-to-Rejected transition"
    );
    store.set_query_only(false).await?;
    assert_eq!(
        store.upload_session(awaiting.key).await?,
        Some(awaiting.clone()),
        "a failed rejection changed the Awaiting row or its stage identity"
    );

    let mut rejected = awaiting.clone();
    rejected.state = StoredUploadState::Rejected;
    assert_eq!(
        store
            .reject_upload_session(
                awaiting.key,
                awaiting.target_path.clone(),
                awaiting.stage_path.clone(),
                Duration::ZERO,
            )
            .await?,
        RejectUploadSession::Rejected(rejected.clone())
    );
    assert!(
        store
            .expired_upload_sessions_page(None, 8)
            .await?
            .iter()
            .any(|snapshot| snapshot.session == rejected)
    );

    store.set_query_only(true).await?;
    assert_eq!(
        store
            .reject_upload_session(
                rejected.key,
                rejected.target_path.clone(),
                rejected.stage_path.clone(),
                TTL,
            )
            .await?,
        RejectUploadSession::Rejected(rejected.clone())
    );
    store.set_query_only(false).await?;
    assert!(
        store
            .expired_upload_sessions_page(None, 8)
            .await?
            .iter()
            .any(|snapshot| snapshot.session == rejected),
        "an idempotent rejected retry refreshed the terminal TTL"
    );
    Ok(())
}

#[tokio::test]
async fn purge_jobs_are_idempotent_bounded_and_owner_scoped() -> Result<()> {
    let store = temporary_with_repository_limits(repository_limits(8, 4, 2, 1))?;
    let first = purge(1, 1);
    assert_eq!(
        store.prepare_purge_job(first.clone()).await?,
        StorePurgeJob::Inserted
    );
    assert_eq!(
        store.prepare_purge_job(first.clone()).await?,
        StorePurgeJob::Existing
    );

    let mut wrong_identity = first.clone();
    wrong_identity.source_identity.inode += 1;
    assert_eq!(
        store.prepare_purge_job(wrong_identity).await?,
        StorePurgeJob::Conflict
    );
    let mut colliding_path = purge(2, 2);
    colliding_path.trash_path = first.trash_path.clone();
    assert_eq!(
        store.prepare_purge_job(colliding_path).await?,
        StorePurgeJob::Conflict
    );
    assert_eq!(
        store.prepare_purge_job(purge(1, 2)).await?,
        StorePurgeJob::Full
    );

    let second = purge(2, 2);
    assert_eq!(
        store.prepare_purge_job(second.clone()).await?,
        StorePurgeJob::Inserted
    );
    assert_eq!(
        store.prepare_purge_job(purge(3, 3)).await?,
        StorePurgeJob::Full
    );
    assert_eq!(
        store.prepared_purge_jobs(8).await?,
        vec![first.clone(), second.clone()]
    );

    let first_revision = [7; 32];
    assert!(
        store
            .mark_purge_job_ready(first.key, first_revision)
            .await?
    );
    assert!(
        store
            .mark_purge_job_ready(first.key, first_revision)
            .await?
    );
    assert!(!store.mark_purge_job_ready(first.key, [8; 32]).await?);
    let mut ready = first.clone();
    ready.state = StoredPurgeState::Ready;
    ready.trash_revision = Some(first_revision);
    assert_eq!(store.purge_jobs(8).await?, vec![ready, second.clone()]);
    let mut claimed = store
        .claim_due_purge_job()
        .await?
        .expect("ready purge job should be claimable");
    claimed.state = StoredPurgeState::Claimed;
    assert_eq!(store.purge_job(first.key).await?, Some(claimed.clone()));
    assert!(store.retry_purge_job(first.key, Duration::ZERO).await?);
    let retried = store
        .claim_due_purge_job()
        .await?
        .expect("retried purge job should be claimable");
    assert_eq!(retried.attempts, 1);
    assert!(store.complete_purge_job(first.key).await?);
    assert!(!store.complete_purge_job(first.key).await?);

    assert_eq!(
        store.prepare_purge_job(purge(3, 3)).await?,
        StorePurgeJob::Inserted
    );
    assert!(store.remove_purge_job(second.key).await?);
    assert!(!store.remove_purge_job(second.key).await?);
    Ok(())
}

#[tokio::test]
async fn maintenance_binding_query_covers_live_state_without_retaining_terminal_uploads()
-> Result<()> {
    let store = temporary_with_repository_limits(repository_limits(16, 16, 8, 8))?;
    for (id, state, expected_bound) in [
        (1, StoredUploadState::Running, true),
        (2, StoredUploadState::CommitStarted, true),
        (3, StoredUploadState::AwaitingConfirmation, true),
        (4, StoredUploadState::Committed, false),
        (5, StoredUploadState::Rejected, false),
        (6, StoredUploadState::Unknown, false),
    ] {
        let mut session = upload(
            1,
            id,
            if state == StoredUploadState::Running {
                4
            } else {
                10
            },
        );
        session.state = state;
        assert_eq!(
            store.save_upload_session(session.clone(), TTL).await?,
            StoreUploadSession::Inserted
        );
        assert_eq!(
            store.state_path_is_bound_blocking(&session.stage_path)?,
            expected_bound,
            "unexpected maintenance binding for {state:?}"
        );
        assert!(
            !store.state_path_is_bound_blocking(&session.target_path)?,
            "upload targets are not maintenance-cleaned internal artifacts"
        );
    }

    let prepared = purge(2, 1);
    let claimed = purge(2, 2);
    let ready = purge(2, 3);
    for job in [&prepared, &claimed, &ready] {
        assert_eq!(
            store.prepare_purge_job(job.clone()).await?,
            StorePurgeJob::Inserted
        );
    }
    assert!(store.mark_purge_job_ready(claimed.key, [7; 32]).await?);
    assert!(store.mark_purge_job_ready(ready.key, [8; 32]).await?);
    assert_eq!(
        store
            .claim_due_purge_job()
            .await?
            .expect("the first ready purge job must be claimable")
            .key,
        claimed.key
    );
    for job in [&prepared, &claimed, &ready] {
        assert!(store.state_path_is_bound_blocking(&job.trash_path)?);
        assert!(!store.state_path_is_bound_blocking(&job.target_path)?);
    }
    assert!(!store.state_path_is_bound_blocking(Path::new("ordinary/file"))?);
    Ok(())
}

#[tokio::test]
async fn state_blocking_paths_are_complete_and_keyset_paginated() -> Result<()> {
    let store = temporary_with_repository_limits(repository_limits(8, 8, 8, 8))?;

    let running = upload(1, 1, 4);
    assert_eq!(
        store.save_upload_session(running.clone(), TTL).await?,
        StoreUploadSession::Inserted
    );
    let mut committing = upload(1, 2, 10);
    committing.state = StoredUploadState::CommitStarted;
    assert_eq!(
        store.save_upload_session(committing.clone(), TTL).await?,
        StoreUploadSession::Inserted
    );
    let mut terminal = upload(1, 3, 10);
    terminal.state = StoredUploadState::Committed;
    assert_eq!(
        store.save_upload_session(terminal, TTL).await?,
        StoreUploadSession::Inserted
    );

    let prepared = purge(2, 1);
    assert_eq!(
        store.prepare_purge_job(prepared.clone()).await?,
        StorePurgeJob::Inserted
    );
    let claimed = purge(2, 2);
    assert_eq!(
        store.prepare_purge_job(claimed.clone()).await?,
        StorePurgeJob::Inserted
    );
    assert!(store.mark_purge_job_ready(claimed.key, [9; 32]).await?);
    assert_eq!(
        store
            .claim_due_purge_job()
            .await?
            .expect("ready purge job should be claimed")
            .key,
        claimed.key
    );
    let ready = purge(2, 3);
    assert_eq!(
        store.prepare_purge_job(ready.clone()).await?,
        StorePurgeJob::Inserted
    );
    assert!(store.mark_purge_job_ready(ready.key, [10; 32]).await?);

    let mut cursor = None;
    let mut paths = Vec::new();
    loop {
        // A one-row page proves the cursor cannot skip the second path
        // belonging to the same owner/id record.
        let page = store
            .state_blocking_paths(cursor, 1, StatePathScanLease::for_test())
            .await?;
        paths.extend(page.paths);
        let Some(next) = page.next else {
            break;
        };
        cursor = Some(next);
    }

    assert_eq!(
        paths,
        vec![
            StateBlockingPath {
                path: running.target_path,
                allows_exact_replacement: true,
            },
            StateBlockingPath {
                path: running.stage_path,
                allows_exact_replacement: false,
            },
            StateBlockingPath {
                path: committing.target_path,
                allows_exact_replacement: false,
            },
            StateBlockingPath {
                path: committing.stage_path,
                allows_exact_replacement: false,
            },
            StateBlockingPath {
                path: prepared.target_path,
                allows_exact_replacement: false,
            },
            StateBlockingPath {
                path: prepared.trash_path,
                allows_exact_replacement: false,
            },
            StateBlockingPath {
                path: claimed.trash_path,
                allows_exact_replacement: false,
            },
            StateBlockingPath {
                path: ready.trash_path,
                allows_exact_replacement: false,
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn upload_and_purge_state_survive_restart_with_unix_path_bytes() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(71, 73);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;

    let mut session = upload(4, 5, 6);
    session.target_path = PathBuf::from(OsString::from_vec(vec![b't', 0xff]));
    session.stage_path = PathBuf::from(OsString::from_vec(vec![b's', 0xfe]));
    session.stage_identity = Some(StoredFileIdentity {
        device: u64::MAX,
        inode: u64::MAX - 1,
    });
    assert_eq!(
        store.save_upload_session(session.clone(), TTL).await?,
        StoreUploadSession::Inserted
    );

    let mut committing = upload(4, 6, 10);
    committing.state = StoredUploadState::CommitStarted;
    committing.stage_identity = Some(StoredFileIdentity {
        device: 17,
        inode: 19,
    });
    assert_eq!(
        store.save_upload_session(committing.clone(), TTL).await?,
        StoreUploadSession::Inserted
    );

    let mut prepared = purge(6, 7);
    prepared.target_path = PathBuf::from(OsString::from_vec(vec![b'p', 0xfd]));
    prepared.trash_path = PathBuf::from(OsString::from_vec(vec![b'q', 0xfc]));
    assert_eq!(
        store.prepare_purge_job(prepared.clone()).await?,
        StorePurgeJob::Inserted
    );
    let trash_revision = [11; 32];
    assert!(
        store
            .mark_purge_job_ready(prepared.key, trash_revision)
            .await?
    );
    let claimed = store
        .claim_due_purge_job()
        .await?
        .expect("purge job should be claimable before restart");
    assert_eq!(claimed.state, StoredPurgeState::Claimed);
    store.shutdown_for_test();

    let reopened = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    assert_eq!(reopened.upload_session(session.key).await?, Some(session));
    committing.state = StoredUploadState::Unknown;
    assert_eq!(
        reopened.upload_session(committing.key).await?,
        Some(committing)
    );
    let mut recovered = prepared;
    recovered.trash_revision = Some(trash_revision);
    recovered.state = StoredPurgeState::Ready;
    assert_eq!(
        reopened.purge_job(recovered.key).await?,
        Some(recovered.clone())
    );
    recovered.state = StoredPurgeState::Claimed;
    assert_eq!(
        reopened.claim_due_purge_job().await?,
        Some(recovered),
        "a claimed job must be reset to immediately-due Ready on reopen"
    );
    Ok(())
}

#[tokio::test]
async fn restart_refreshes_an_expired_commit_started_upload_barrier() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(79, 83);
    let recovery_ttl = Duration::from_secs(60);
    let store =
        StateStore::open_with_upload_ttl(&path, &identity, CAPACITY, PER_OWNER, TTL, recovery_ttl)?;

    let mut committing = upload(8, 9, 10);
    committing.state = StoredUploadState::CommitStarted;
    assert_eq!(
        store
            .save_upload_session(committing.clone(), Duration::ZERO)
            .await?,
        StoreUploadSession::Inserted
    );
    assert!(
        store
            .expired_upload_sessions_page(None, 8)
            .await?
            .is_empty(),
        "CommitStarted must remain excluded even after its original TTL elapsed"
    );
    store.shutdown_for_test();

    let reopened =
        StateStore::open_with_upload_ttl(&path, &identity, CAPACITY, PER_OWNER, TTL, recovery_ttl)?;
    committing.state = StoredUploadState::Unknown;
    assert_eq!(
        reopened.upload_session(committing.key).await?,
        Some(committing)
    );
    assert!(
        reopened
            .expired_upload_sessions_page(None, 8)
            .await?
            .is_empty(),
        "recovery must grant the ambiguity barrier a fresh upload-session TTL"
    );
    Ok(())
}

#[tokio::test]
async fn restart_caps_future_upload_deadlines_after_a_clock_rollback() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(89, 97);
    let store = StateStore::open_with_upload_ttl(&path, &identity, CAPACITY, PER_OWNER, TTL, TTL)?;

    let running = upload(9, 1, 4);
    let mut awaiting = upload(9, 2, 10);
    awaiting.state = StoredUploadState::AwaitingConfirmation;
    let mut rejected = upload(9, 3, 10);
    rejected.state = StoredUploadState::Rejected;
    let mut committing = upload(9, 4, 10);
    committing.state = StoredUploadState::CommitStarted;
    for session in [&running, &awaiting, &rejected, &committing] {
        assert_eq!(
            store.save_upload_session(session.clone(), TTL).await?,
            StoreUploadSession::Inserted
        );
    }
    store.shutdown_for_test();

    let connection = Connection::open(&path)?;
    assert_eq!(
        connection.execute("UPDATE upload_sessions SET expires_at_ms = ?1", [i64::MAX])?,
        4
    );
    drop(connection);

    let reopened = StateStore::open_with_upload_ttl(
        &path,
        &identity,
        CAPACITY,
        PER_OWNER,
        TTL,
        Duration::ZERO,
    )?;
    let expired = reopened.expired_upload_sessions_page(None, 8).await?;
    assert_eq!(
        expired.len(),
        4,
        "persisted future upload deadlines must be bounded on restart"
    );
    for expected in [running, awaiting, rejected] {
        assert!(
            expired.iter().any(|snapshot| snapshot.session == expected),
            "a non-committing upload state kept its future deadline: {expected:?}"
        );
    }
    committing.state = StoredUploadState::Unknown;
    assert!(
        expired
            .iter()
            .any(|snapshot| snapshot.session == committing),
        "CommitStarted recovery must still preserve its ambiguity barrier"
    );
    Ok(())
}

#[tokio::test]
async fn restart_makes_future_purge_retries_due_after_a_clock_rollback() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(101, 103);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;

    let claimed = purge(10, 1);
    assert_eq!(
        store.prepare_purge_job(claimed.clone()).await?,
        StorePurgeJob::Inserted
    );
    assert!(store.mark_purge_job_ready(claimed.key, [1; 32]).await?);
    assert_eq!(
        store
            .claim_due_purge_job()
            .await?
            .expect("the first purge should be claimed")
            .key,
        claimed.key
    );

    let retried = purge(10, 2);
    assert_eq!(
        store.prepare_purge_job(retried.clone()).await?,
        StorePurgeJob::Inserted
    );
    assert!(store.mark_purge_job_ready(retried.key, [2; 32]).await?);
    assert_eq!(
        store
            .claim_due_purge_job()
            .await?
            .expect("the second purge should be claimed")
            .key,
        retried.key
    );
    assert!(
        store
            .retry_purge_job(retried.key, Duration::from_secs(30))
            .await?
    );

    let prepared = purge(10, 3);
    assert_eq!(
        store.prepare_purge_job(prepared.clone()).await?,
        StorePurgeJob::Inserted
    );
    store.shutdown_for_test();

    let connection = Connection::open(&path)?;
    assert_eq!(
        connection.execute(
            "UPDATE purge_jobs SET next_attempt_at_ms = ?1 WHERE state IN (?2, ?3)",
            params![i64::MAX, PURGE_READY, PURGE_CLAIMED]
        )?,
        2
    );
    drop(connection);

    let reopened = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    let recovered_claimed = reopened
        .claim_due_purge_job()
        .await?
        .expect("a recovered Claimed purge must be immediately due");
    assert_eq!(recovered_claimed.key, claimed.key);
    assert_eq!(recovered_claimed.attempts, 0);
    assert!(reopened.complete_purge_job(claimed.key).await?);

    let recovered_retry = reopened
        .claim_due_purge_job()
        .await?
        .expect("a future Ready purge must be made due on restart");
    assert_eq!(recovered_retry.key, retried.key);
    assert_eq!(recovered_retry.attempts, 1);
    assert!(reopened.complete_purge_job(retried.key).await?);
    assert_eq!(reopened.claim_due_purge_job().await?, None);
    assert_eq!(reopened.purge_job(prepared.key).await?, Some(prepared));
    Ok(())
}

#[test]
fn rejects_old_application_version_without_modification() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("old-version.sqlite3");
    let identity = root(211, 223);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    store.shutdown_for_test();

    let connection = Connection::open(&path)?;
    connection.execute(
        "UPDATE product_metadata SET application_version = '0.49.7' WHERE singleton = 1",
        [],
    )?;
    drop(connection);
    fs::set_permissions(&path, Permissions::from_mode(0o640))?;
    let files_before = state_database_files_snapshot(&path)?;

    let error = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)
        .err()
        .expect("an old application version must be rejected");
    assert!(
        format!("{error:#}").contains("application version is not exactly current"),
        "unexpected old-version error: {error:#}"
    );
    assert_eq!(state_database_files_snapshot(&path)?, files_before);
    Ok(())
}

#[test]
fn rejects_unmarked_state_database_without_modification() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("unmarked.sqlite3");
    let identity = root(227, 229);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    store.shutdown_for_test();

    let connection = Connection::open(&path)?;
    connection.execute_batch("DROP TABLE product_metadata;")?;
    drop(connection);
    fs::set_permissions(&path, Permissions::from_mode(0o640))?;
    let files_before = state_database_files_snapshot(&path)?;

    let error = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)
        .err()
        .expect("an unmarked former state database must be rejected");
    assert!(
        format!("{error:#}").contains("does not exactly match the current DUFS schema"),
        "unexpected unmarked-database error: {error:#}"
    );
    assert_eq!(state_database_files_snapshot(&path)?, files_before);
    Ok(())
}

#[test]
fn rejects_raw_schema_fingerprint_drift_without_modification() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("fingerprint-drift.sqlite3");
    let identity = root(233, 239);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    store.shutdown_for_test();

    let connection = Connection::open(&path)?;
    connection.pragma_update(None, "writable_schema", true)?;
    assert_eq!(
        connection.execute(
            "UPDATE sqlite_schema
                SET sql = replace(sql, 'CREATE INDEX operations_expiry',
                                   'CREATE  INDEX operations_expiry')
              WHERE name = 'operations_expiry'",
            [],
        )?,
        1
    );
    connection.pragma_update(None, "writable_schema", false)?;
    let schema_cookie: i64 =
        connection.pragma_query_value(None, "schema_version", |row| row.get(0))?;
    connection.pragma_update(None, "schema_version", schema_cookie + 1)?;
    drop(connection);
    fs::set_permissions(&path, Permissions::from_mode(0o640))?;
    let files_before = state_database_files_snapshot(&path)?;

    let error = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)
        .err()
        .expect("raw schema SQL drift must be rejected");
    assert!(
        format!("{error:#}").contains("schema fingerprint does not match"),
        "unexpected fingerprint-drift error: {error:#}"
    );
    assert_eq!(state_database_files_snapshot(&path)?, files_before);
    Ok(())
}

#[test]
fn rejects_tampered_current_column_constraints_and_index_predicates() -> Result<()> {
    let cases = [
        (
            "declared type",
            "store_meta",
            "value BLOB NOT NULL",
            "value TEXT NOT NULL",
        ),
        (
            "not-null flag",
            "store_meta",
            "value BLOB NOT NULL",
            "value BLOB",
        ),
        (
            "default value",
            "store_meta",
            "value BLOB NOT NULL",
            "value BLOB NOT NULL DEFAULT X''",
        ),
        (
            "primary key order",
            "operations",
            "PRIMARY KEY(owner_digest, operation_id)",
            "PRIMARY KEY(operation_id, owner_digest)",
        ),
        (
            "check constraint",
            "operations",
            "CHECK(length(owner_digest) = 32)",
            "CHECK(length(owner_digest) <= 32)",
        ),
        (
            "partial-index predicate",
            "operations_expiry",
            "WHERE state = 2",
            "WHERE state = 1",
        ),
    ];

    for (case_index, (label, object, original, replacement)) in cases.into_iter().enumerate() {
        let directory = tempdir()?;
        let path = directory
            .path()
            .join(format!("tampered-current-{case_index}.sqlite3"));
        let identity = root(500 + case_index as u64, 600 + case_index as u64);
        let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
        store.shutdown_for_test();

        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "writable_schema", true)?;
        assert_eq!(
            connection.execute(
                "UPDATE sqlite_schema
                    SET sql = replace(sql, ?2, ?3)
                  WHERE name = ?1 AND instr(sql, ?2) > 0",
                params![object, original, replacement],
            )?,
            1,
            "the {label} fixture must alter exactly one schema object"
        );
        connection.pragma_update(None, "writable_schema", false)?;
        let schema_cookie: i64 =
            connection.pragma_query_value(None, "schema_version", |row| row.get(0))?;
        connection.pragma_update(None, "schema_version", schema_cookie + 1)?;
        drop(connection);
        fs::set_permissions(&path, Permissions::from_mode(0o640))?;

        let bytes_before = fs::read(&path)?;
        let schema_before = database_schema_snapshot(&path)?;
        let result = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL);
        let error = result
            .err()
            .expect("a current database with a tampered contract must be rejected");
        let message = format!("{error:#}");
        assert!(
            message.contains("does not exactly match the current DUFS schema"),
            "unexpected {label} error: {message}"
        );
        assert_eq!(fs::read(&path)?, bytes_before);
        assert_eq!(database_schema_snapshot(&path)?, schema_before);
        assert_eq!(fs::metadata(&path)?.mode() & 0o777, 0o640);
    }
    Ok(())
}

#[test]
fn rejects_unsafe_sqlite_sidecars_before_creating_the_main_database() -> Result<()> {
    for (suffix, kind, expected_error) in [
        ("-journal", "symlink", "cannot be a symbolic link"),
        ("-wal", "directory", "must be a regular file"),
        ("-shm", "hard-link", "cannot have multiple hard links"),
    ] {
        let directory = tempdir()?;
        let path = directory.path().join("state.sqlite3");
        let mut sidecar_name = path.as_os_str().to_os_string();
        sidecar_name.push(suffix);
        let sidecar = PathBuf::from(sidecar_name);
        let auxiliary = directory.path().join("auxiliary");
        match kind {
            "symlink" => {
                fs::write(&auxiliary, b"symlink target must remain unchanged")?;
                symlink(&auxiliary, &sidecar)?;
            }
            "directory" => fs::create_dir(&sidecar)?,
            "hard-link" => {
                fs::write(&sidecar, b"hard-linked sidecar must remain unchanged")?;
                fs::hard_link(&sidecar, &auxiliary)?;
            }
            _ => unreachable!("the sidecar fixture kind is fixed"),
        }

        let result = StateStore::open(&path, &root(701, 709), CAPACITY, PER_OWNER, TTL);
        let error = result
            .err()
            .expect("an unsafe SQLite sidecar must fail before database creation");
        assert!(
            format!("{error:#}").contains(expected_error),
            "unexpected {kind} error: {error:#}"
        );
        assert!(
            fs::symlink_metadata(&path).is_err_and(|error| error.kind() == ErrorKind::NotFound),
            "the main database was created for an unsafe {kind} sidecar"
        );
        match kind {
            "symlink" => {
                assert!(fs::symlink_metadata(&sidecar)?.file_type().is_symlink());
                assert_eq!(
                    fs::read(&auxiliary)?,
                    b"symlink target must remain unchanged"
                );
            }
            "directory" => assert!(fs::symlink_metadata(&sidecar)?.is_dir()),
            "hard-link" => {
                assert_eq!(fs::metadata(&sidecar)?.nlink(), 2);
                assert_eq!(
                    fs::read(&sidecar)?,
                    b"hard-linked sidecar must remain unchanged"
                );
            }
            _ => unreachable!("the sidecar fixture kind is fixed"),
        }
    }
    Ok(())
}

#[test]
fn rejects_an_orphan_regular_sidecar_without_creating_the_main_database() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let wal = directory.path().join("state.sqlite3-wal");
    fs::write(&wal, b"orphan regular WAL must remain unchanged")?;
    fs::set_permissions(&wal, Permissions::from_mode(0o600))?;
    let wal_before = state_database_files_snapshot(&path)?;

    let result = StateStore::open(&path, &root(711, 713), CAPACITY, PER_OWNER, TTL);
    let error = result
        .err()
        .expect("an orphan regular sidecar must not initialize a main database");
    assert!(format!("{error:#}").contains("while a SQLite sidecar already exists"));
    assert!(fs::symlink_metadata(&path).is_err_and(|error| error.kind() == ErrorKind::NotFound));
    assert_eq!(state_database_files_snapshot(&path)?, wal_before);
    Ok(())
}

#[test]
fn detects_a_sidecar_replacement_before_sqlite_can_open_it() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let sidecar = directory.path().join("state.sqlite3-journal");
    let original = directory.path().join("original-journal");
    fs::write(&path, b"main database bytes must remain unchanged")?;
    fs::write(&sidecar, b"validated journal")?;

    let main_before = fs::read(&path)?;
    let result = database::open_existing_database_after_sidecar_snapshot_for_test(
        &path,
        root(719, 727),
        || {
            fs::rename(&sidecar, &original)?;
            fs::write(&sidecar, b"replacement journal")?;
            Ok(())
        },
    );
    let error = result
        .expect_err("a sidecar replacement must be rejected before SQLite opens the database");
    let error = format!("{error:#}");
    assert!(
        error.contains("SQLite sidecar")
            && (error.contains("changed identity or security metadata")
                || error.contains("replaced after validation")),
        "unexpected sidecar replacement error: {error}"
    );
    assert_eq!(fs::read(&path)?, main_before);
    assert_eq!(fs::read(&original)?, b"validated journal");
    assert_eq!(fs::read(&sidecar)?, b"replacement journal");
    Ok(())
}

#[test]
fn raw_main_snapshot_copy_enforces_its_limit_before_and_during_copy() -> Result<()> {
    let directory = tempdir()?;
    let source = directory.path().join("state.sqlite3");
    let oversized_snapshot = directory.path().join("oversized.snapshot");
    fs::write(&source, [b'x'; 64])?;
    let mut destination = File::create(&oversized_snapshot)?;
    let error = database::copy_raw_main_database_snapshot_after_inspect_for_test(
        &source,
        &mut destination,
        8,
        || Ok(()),
    )
    .expect_err("an already oversized raw database was copied");
    assert!(
        format!("{error:#}").contains("exceeds the raw validation snapshot limit of 8 bytes"),
        "unexpected oversized snapshot error: {error:#}"
    );
    assert_eq!(destination.metadata()?.len(), 0);

    fs::write(&source, [b'y'; 8])?;
    let raced_snapshot = directory.path().join("raced.snapshot");
    let mut destination = File::create(&raced_snapshot)?;
    let error = database::copy_raw_main_database_snapshot_after_inspect_for_test(
        &source,
        &mut destination,
        8,
        || {
            OpenOptions::new()
                .append(true)
                .open(&source)?
                .write_all(&[b'z'; 56])?;
            Ok(())
        },
    )
    .expect_err("a database growth race crossed the snapshot copy budget");
    assert!(
        format!("{error:#}").contains("grew beyond the raw validation snapshot limit of 8 bytes"),
        "unexpected growth-race error: {error:#}"
    );
    assert_eq!(destination.metadata()?.len(), 9);

    fs::write(&source, [b'w'; 8])?;
    let exact_snapshot = directory.path().join("exact.snapshot");
    let mut destination = File::create(&exact_snapshot)?;
    database::copy_raw_main_database_snapshot_after_inspect_for_test(
        &source,
        &mut destination,
        8,
        || Ok(()),
    )?;
    assert_eq!(fs::read(exact_snapshot)?, [b'w'; 8]);
    Ok(())
}

#[test]
fn raw_main_validation_rejects_a_valid_current_wal_shadow_without_modification() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(733, 739);
    let mut connection = Connection::open(&path)?;
    connection.execute_batch(
        "CREATE TABLE foreign_records(id INTEGER PRIMARY KEY, value TEXT NOT NULL) STRICT;
         INSERT INTO foreign_records(value) VALUES ('foreign main must survive');",
    )?;
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    assert!(mode.eq_ignore_ascii_case("wal"));
    connection.pragma_update(None, "wal_autocheckpoint", 0_i64)?;

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch("DROP TABLE foreign_records;")?;
    transaction.execute_batch(database::CURRENT_SCHEMA)?;
    transaction.execute(
        "INSERT INTO product_metadata(
             singleton, application, application_version, schema_revision, schema_sha256
         ) VALUES (1, ?1, ?2, ?3, ?4)",
        params![
            APPLICATION,
            env!("CARGO_PKG_VERSION"),
            CURRENT_SCHEMA_REVISION,
            database::expected_schema_fingerprint()?
        ],
    )?;
    transaction.execute(
        "INSERT INTO store_meta(key, value) VALUES
         ('root-device-be', ?1), ('root-inode-be', ?2)",
        params![
            identity.device.to_be_bytes().as_slice(),
            identity.inode.to_be_bytes().as_slice()
        ],
    )?;
    transaction.commit()?;

    let wal = directory.path().join("state.sqlite3-wal");
    let shm = directory.path().join("state.sqlite3-shm");
    assert!(fs::metadata(&wal)?.is_file());
    assert!(fs::metadata(&shm)?.is_file());
    fs::set_permissions(&path, Permissions::from_mode(0o640))?;
    let files_before = state_database_files_snapshot(&path)?;

    let result = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL);
    let error = result
        .err()
        .expect("a current DUFS WAL must not be allowed to shadow a foreign main database");
    assert!(
        format!("{error:#}").contains("raw main state database"),
        "unexpected WAL-shadow error: {error:#}"
    );
    assert_eq!(state_database_files_snapshot(&path)?, files_before);
    drop(connection);
    Ok(())
}

#[test]
fn hot_rollback_baseline_is_rejected_without_recovery_or_modification() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(743, 751);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    store.shutdown_for_test();

    let status = ProcessCommand::new(std::env::current_exe()?)
        .arg("sqlite_hot_rollback_crash_fixture_helper")
        .arg("--test-threads=1")
        .env(HOT_ROLLBACK_FIXTURE_PATH, path.as_os_str())
        .status()?;
    assert_eq!(status.code(), Some(86));
    let journal = directory.path().join("state.sqlite3-journal");
    assert!(fs::metadata(&journal)?.is_file());
    assert_eq!(fs::metadata(&journal)?.nlink(), 1);
    fs::set_permissions(&path, Permissions::from_mode(0o640))?;
    let files_before = state_database_files_snapshot(&path)?;

    let result = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL);
    let error = result
        .err()
        .expect("a hot rollback database with an untrusted raw baseline must be rejected");
    assert!(
        format!("{error:#}").contains("raw main state database"),
        "unexpected hot-rollback error: {error:#}"
    );
    assert_eq!(state_database_files_snapshot(&path)?, files_before);
    Ok(())
}

#[test]
fn trusted_hot_rollback_journal_is_recovered_before_preflight() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(757, 761);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    store.shutdown_for_test();

    let status = ProcessCommand::new(std::env::current_exe()?)
        .arg("sqlite_hot_rollback_crash_fixture_helper")
        .arg("--test-threads=1")
        .env(HOT_ROLLBACK_FIXTURE_PATH, path.as_os_str())
        .env(HOT_ROLLBACK_FIXTURE_KIND, HOT_ROLLBACK_ORDINARY_OPERATION)
        .status()?;
    assert_eq!(status.code(), Some(86));
    let journal = directory.path().join("state.sqlite3-journal");
    assert!(fs::metadata(&journal)?.is_file());
    assert_eq!(fs::metadata(&journal)?.nlink(), 1);
    fs::set_permissions(&path, Permissions::from_mode(0o640))?;

    let reopened = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    reopened.shutdown_for_test();
    assert_eq!(fs::metadata(&path)?.mode() & 0o777, 0o600);
    assert!(
        fs::symlink_metadata(&journal).is_err_and(|error| error.kind() == ErrorKind::NotFound),
        "successful recovery must consume the hot rollback journal"
    );
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let operation_count: i64 =
        connection.query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))?;
    assert_eq!(
        operation_count, 0,
        "the uncommitted operation must be rolled back during startup recovery"
    );
    Ok(())
}

#[test]
fn rollback_journal_that_restores_an_untrusted_root_is_rejected_without_modification() -> Result<()>
{
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(
        HOT_ROLLBACK_TRUSTED_ROOT_DEVICE,
        HOT_ROLLBACK_TRUSTED_ROOT_INODE,
    );
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    store.shutdown_for_test();

    // Commit an untrusted root first. The crash transaction below flushes the
    // expected root into the raw main file, while its journal still restores
    // this committed untrusted identity.
    let connection = Connection::open(&path)?;
    connection.execute(
        "UPDATE store_meta
            SET value = CASE key
                WHEN 'root-device-be' THEN ?1
                ELSE ?2
            END
          WHERE key IN ('root-device-be', 'root-inode-be')",
        params![
            1_u64.to_be_bytes().as_slice(),
            2_u64.to_be_bytes().as_slice()
        ],
    )?;
    drop(connection);

    let status = ProcessCommand::new(std::env::current_exe()?)
        .arg("sqlite_hot_rollback_crash_fixture_helper")
        .arg("--test-threads=1")
        .env(HOT_ROLLBACK_FIXTURE_PATH, path.as_os_str())
        .env(HOT_ROLLBACK_FIXTURE_KIND, HOT_ROLLBACK_TRUSTED_MAIN)
        .status()?;
    assert_eq!(status.code(), Some(86));
    let files_before = state_database_files_snapshot(&path)?;

    let error = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)
        .err()
        .expect("a journal that rolls back to an untrusted root must be rejected");
    assert!(
        format!("{error:#}").contains("privately recovered rollback-journal snapshot"),
        "the trusted raw main must be rejected only after private recovery: {error:#}"
    );
    assert_eq!(state_database_files_snapshot(&path)?, files_before);
    Ok(())
}

#[test]
fn rejects_foreign_database_without_changing_mode_bytes_schema_or_journal() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("foreign.sqlite3");
    {
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE foreign_records(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO foreign_records(value) VALUES ('must remain untouched');",
        )?;
        let mode: String =
            connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        assert!(mode.eq_ignore_ascii_case("wal"));
    }
    fs::set_permissions(&path, Permissions::from_mode(0o640))?;
    let bytes_before = fs::read(&path)?;
    let mode_before = fs::metadata(&path)?.mode() & 0o777;
    let schema_before = database_schema_snapshot(&path)?;

    let result = StateStore::open(&path, &root(1, 2), CAPACITY, PER_OWNER, TTL);
    let error = result.err().expect("a foreign database must be rejected");
    assert!(
        format!("{error:#}").contains("does not exactly match the current DUFS schema"),
        "unexpected foreign-database error: {error:#}"
    );

    assert_eq!(fs::read(&path)?, bytes_before);
    assert_eq!(fs::metadata(&path)?.mode() & 0o777, mode_before);
    assert_eq!(database_schema_snapshot(&path)?, schema_before);
    assert!(schema_before.0.eq_ignore_ascii_case("wal"));
    Ok(())
}

#[test]
fn rejects_non_sqlite_file_without_changing_mode_or_bytes() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("not-sqlite.sqlite3");
    let contents = b"this is owned by another application\n";
    fs::write(&path, contents)?;
    fs::set_permissions(&path, Permissions::from_mode(0o640))?;
    let mode_before = fs::metadata(&path)?.mode() & 0o777;

    assert!(StateStore::open(&path, &root(1, 2), CAPACITY, PER_OWNER, TTL).is_err());
    assert_eq!(fs::read(&path)?, contents);
    assert_eq!(fs::metadata(&path)?.mode() & 0o777, mode_before);
    Ok(())
}

#[tokio::test]
async fn initializes_a_preexisting_empty_database_after_read_only_preflight() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("empty.sqlite3");
    fs::write(&path, [])?;
    fs::set_permissions(&path, Permissions::from_mode(0o644))?;

    let store = StateStore::open(&path, &root(107, 109), CAPACITY, PER_OWNER, TTL)?;
    let pragmas = store.inspect_pragmas().await?;
    assert_eq!(pragmas.application, APPLICATION);
    assert_eq!(pragmas.application_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(pragmas.schema_revision, CURRENT_SCHEMA_REVISION);
    assert_eq!(
        pragmas.schema_sha256,
        database::expected_schema_fingerprint()?
    );
    assert_eq!(fs::metadata(&path)?.mode() & 0o777, 0o600);
    store.shutdown_for_test();
    Ok(())
}

#[tokio::test]
async fn bounded_queue_keeps_control_commands_and_shutdown_available() -> Result<()> {
    let store =
        StateStore::temporary_with_limits_for_test(CAPACITY, PER_OWNER, TTL, 1, REPOSITORY_LIMITS)?;
    let operation = key(9, 1);
    let lease = match store.begin_operation(operation, fingerprint(9)).await? {
        StoreBegin::Started { lease } => lease,
        other => panic!("unexpected begin result: {other:?}"),
    };

    let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
    let (release_sender, release_receiver) = mpsc::sync_channel(0);
    store.send(Command::Block {
        entered: entered_sender,
        release: release_receiver,
    })?;
    entered_receiver.recv_timeout(Duration::from_secs(1))?;

    let (queued_reply, queued_receiver) = oneshot::channel();
    store.send(Command::Status {
        key: key(9, 9),
        reply: queued_reply,
    })?;
    let error = store
        .operation_status(key(9, 8))
        .await
        .expect_err("a saturated bounded queue must reject new work");
    assert_eq!(
        error.downcast_ref::<StateStoreDispatchError>(),
        Some(&StateStoreDispatchError::QueueFull)
    );
    assert!(store.is_healthy());

    // Abandon uses the separate control channel, so cleanup cannot be
    // dropped merely because the regular command queue is saturated.
    store.abandon_operation(operation, lease);
    release_sender.send(())?;
    assert_eq!(queued_receiver.await??, StoreStatus::NotFound);
    assert_eq!(
        store.operation_status(operation).await?,
        StoreStatus::NotFound
    );

    let clone = store.clone();
    store.close().await?;
    clone.close().await?;
    assert!(!clone.is_healthy());
    let error = clone
        .operation_status(operation)
        .await
        .expect_err("a closed store must reject dispatch");
    assert_eq!(
        error.downcast_ref::<StateStoreDispatchError>(),
        Some(&StateStoreDispatchError::Unavailable)
    );
    Ok(())
}

#[tokio::test]
async fn disk_database_has_expected_identity_permissions_and_pragmas() -> Result<()> {
    let directory = tempdir()?;
    let path = directory.path().join("state.sqlite3");
    let identity = root(7, 11);
    let store = StateStore::open(&path, &identity, CAPACITY, PER_OWNER, TTL)?;
    let pragmas = store.inspect_pragmas().await?;
    assert_eq!(pragmas.journal_mode.to_ascii_lowercase(), "delete");
    assert_eq!(pragmas.synchronous, 3);
    assert_eq!(pragmas.foreign_keys, 1);
    assert_eq!(pragmas.trusted_schema, 0);
    assert_eq!(pragmas.mmap_size, 0);
    assert_eq!(pragmas.application, APPLICATION);
    assert_eq!(pragmas.application_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(pragmas.schema_revision, CURRENT_SCHEMA_REVISION);
    assert_eq!(
        pragmas.schema_sha256,
        database::expected_schema_fingerprint()?
    );
    assert_eq!(fs::metadata(&path)?.mode() & 0o777, 0o600);
    store.shutdown_for_test();

    let mismatch = StateStore::open(
        &path,
        &root(identity.device, identity.inode + 1),
        CAPACITY,
        PER_OWNER,
        TTL,
    );
    assert!(mismatch.is_err());
    Ok(())
}

#[test]
fn store_clock_keeps_advancing_across_wall_clock_changes() -> Result<()> {
    let anchor = Instant::now();
    let mut clock = StoreClock {
        wall_anchor_ms: 1_000,
        monotonic_anchor: anchor,
        last_ms: 1_000,
    };

    assert_eq!(
        clock.observe(Some(100), anchor + Duration::from_millis(100))?,
        1_100,
        "a wall-clock rollback must not freeze relative deadlines"
    );
    assert_eq!(
        clock.observe(None, anchor + Duration::from_millis(200))?,
        1_200,
        "a transient wall-clock read failure must use monotonic time"
    );
    assert_eq!(
        clock.observe(Some(10_000), anchor + Duration::from_millis(300))?,
        10_000,
        "a forward correction should advance persisted timestamps"
    );
    assert_eq!(
        clock.observe(Some(500), anchor + Duration::from_millis(450))?,
        10_150,
        "a rollback after a forward correction must resume from the correction"
    );
    Ok(())
}

#[test]
fn rejects_symbolic_link_database() -> Result<()> {
    let directory = tempdir()?;
    let target = directory.path().join("target.sqlite3");
    fs::write(&target, [])?;
    let link = directory.path().join("state.sqlite3");
    symlink(&target, &link)?;
    let result = StateStore::open(&link, &root(1, 1), CAPACITY, PER_OWNER, TTL);
    assert!(result.is_err());
    Ok(())
}

#[test]
fn rejects_invalid_limits_and_outcomes() -> Result<()> {
    assert!(StateStore::temporary_for_test(0, 0, TTL).is_err());
    assert!(StateStore::temporary_for_test(1, 2, TTL).is_err());
    assert!(
        StateStore::temporary_with_limits_for_test(
            CAPACITY,
            PER_OWNER,
            TTL,
            1,
            repository_limits(0, 0, 1, 1),
        )
        .is_err()
    );
    assert!(
        StateStore::temporary_with_limits_for_test(
            CAPACITY,
            PER_OWNER,
            TTL,
            1,
            repository_limits(1, 1, 1, 2),
        )
        .is_err()
    );
    let invalid = StoredOutcome {
        status: 204,
        state: StoredTerminalState::Failed,
        code: Some("failed".to_string()),
    };
    assert!(invalid.validate().is_err());
    let mut invalid_upload = upload(1, 1, 5);
    invalid_upload.state = StoredUploadState::CommitStarted;
    assert!(invalid_upload.validate().is_err());
    let mut invalid_purge = purge(1, 1);
    invalid_purge.trash_path = invalid_purge.target_path.clone();
    assert!(invalid_purge.validate_new().is_err());
    Ok(())
}
