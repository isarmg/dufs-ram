use super::*;

mod claim_tests {
    use super::*;

    #[test]
    fn cleanup_claim_releases_the_registry_lock_while_preserving_exclusion() {
        let temp = assert_fs::TempDir::new().unwrap();
        let entry = temp.path().join("candidate");
        std::fs::write(&entry, "stale").unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let entry_key = rooted_fs.entry_key_blocking(&entry).unwrap();
        let active = Mutex::new(HashSet::new());

        let claim = try_claim_stale_entry(&active, &entry_key).expect("claim stale entry");
        assert!(
            active.try_lock().is_ok(),
            "a cleanup claim must not hold the registry mutex across filesystem I/O"
        );
        assert!(
            try_claim_stale_entry(&active, &entry_key).is_none(),
            "a second cleanup must observe the in-memory marker"
        );
        assert!(
            active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&entry_key.maintenance_marker()),
            "upload admission must be able to observe the cleanup marker"
        );

        drop(claim);
        assert!(
            active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "dropping the cleanup claim must release its marker"
        );
    }
}

mod sqlite_cleanup_tests {
    use super::*;
    use crate::server::{
        identity::OwnerId,
        state_store::{
            PurgeJobKey, StorePurgeJob, StoreUploadSession, StoredPurgeJob, StoredPurgeState,
            UploadSessionKey,
        },
    };
    use std::os::unix::fs::MetadataExt;
    use tokio::io::AsyncWriteExt;
    use uuid::Uuid;

    const EXPIRED_TTL: Duration = Duration::from_millis(100);

    fn temporary_store() -> StateStore {
        StateStore::temporary_for_test(32, 16, Duration::from_secs(60)).unwrap()
    }

    fn stored_session(
        rooted_fs: &RootedFs,
        owner: [u8; 32],
        upload_id: Uuid,
        target: &Path,
        stage: &Path,
        state: StoredUploadState,
        stage_identity: Option<StoredFileIdentity>,
    ) -> StoredUploadSession {
        let upload_length = 8;
        StoredUploadSession {
            key: UploadSessionKey {
                owner,
                id: *upload_id.as_bytes(),
            },
            target_path: rooted_fs.state_relative_path(target).unwrap(),
            stage_path: rooted_fs.state_relative_path(stage).unwrap(),
            upload_length,
            durable_offset: if matches!(
                state,
                StoredUploadState::CommitStarted
                    | StoredUploadState::Committed
                    | StoredUploadState::Unknown
                    | StoredUploadState::AwaitingConfirmation
            ) {
                upload_length
            } else {
                4
            },
            state,
            stage_identity,
            target_revision: None,
        }
    }

    async fn expire() {
        tokio::time::sleep(Duration::from_millis(125)).await;
    }

    async fn create_stage(rooted_fs: &RootedFs, stage: &Path, contents: &[u8]) -> tokio::fs::File {
        let (mut file, _) = rooted_fs.create_private_new(stage).await.unwrap();
        file.write_all(contents).await.unwrap();
        file
    }

    #[tokio::test]
    async fn sqlite_expiry_cleanup_obeys_its_row_limit() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let store = temporary_store();
        let mut keys = Vec::new();
        for index in 0..3 {
            let upload_id = Uuid::new_v4();
            let target = temp.path().join(format!("terminal-{index}.bin"));
            let stage = upload_temp_path(&target, upload_id)?;
            let session = stored_session(
                &rooted_fs,
                [index + 1; 32],
                upload_id,
                &target,
                &stage,
                StoredUploadState::Committed,
                None,
            );
            keys.push(session.key);
            assert_eq!(
                store.save_upload_session(session, EXPIRED_TTL).await?,
                StoreUploadSession::Inserted
            );
        }
        expire().await;

        let first = cleanup_expired_upload_sessions_batch(
            &rooted_fs,
            &store,
            &Mutex::new(HashSet::new()),
            &CancellationToken::new(),
            2,
        )
        .await?;
        assert_eq!(first.examined, 2);
        assert_eq!(first.removed_records, 2);
        assert_eq!(first.removed_stages, 0);
        let mut remaining = 0;
        for key in &keys {
            remaining += usize::from(store.upload_session(*key).await?.is_some());
        }
        assert_eq!(remaining, 1);

        let second = cleanup_expired_upload_sessions_batch(
            &rooted_fs,
            &store,
            &Mutex::new(HashSet::new()),
            &CancellationToken::new(),
            2,
        )
        .await?;
        assert_eq!(second.examined, 1);
        assert_eq!(second.removed_records, 1);
        Ok(())
    }

    #[tokio::test]
    async fn running_expiry_removes_only_the_recorded_stage() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let store = temporary_store();
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("matching.bin");
        let stage = upload_temp_path(&target, upload_id)?;
        let owner = OwnerId::persistent("matching-owner").into_bytes();
        let file = create_stage(&rooted_fs, &stage, b"part").await;
        let metadata = file.metadata().await?;
        let session = stored_session(
            &rooted_fs,
            owner,
            upload_id,
            &target,
            &stage,
            StoredUploadState::Running,
            Some(StoredFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
        );
        let key = session.key;
        assert_eq!(
            store.save_upload_session(session, EXPIRED_TTL).await?,
            StoreUploadSession::Inserted
        );
        drop(file);
        expire().await;

        let batch = cleanup_expired_upload_sessions_batch(
            &rooted_fs,
            &store,
            &Mutex::new(HashSet::new()),
            &CancellationToken::new(),
            8,
        )
        .await?;
        assert_eq!(batch.examined, 1);
        assert_eq!(batch.removed_records, 1);
        assert_eq!(batch.removed_stages, 1);
        assert!(!stage.exists());
        assert!(store.upload_session(key).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn rejected_expiry_finishes_cancelled_discard_cleanup() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let store = temporary_store();
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("rejected.bin");
        let stage = upload_temp_path(&target, upload_id)?;
        let owner = OwnerId::persistent("rejected-owner").into_bytes();
        let file = create_stage(&rooted_fs, &stage, b"complete").await;
        let metadata = file.metadata().await?;
        let mut session = stored_session(
            &rooted_fs,
            owner,
            upload_id,
            &target,
            &stage,
            StoredUploadState::Rejected,
            Some(StoredFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
        );
        session.durable_offset = session.upload_length;
        let key = session.key;
        assert_eq!(
            store.save_upload_session(session, EXPIRED_TTL).await?,
            StoreUploadSession::Inserted
        );
        drop(file);
        expire().await;

        let batch = cleanup_expired_upload_sessions_batch(
            &rooted_fs,
            &store,
            &Mutex::new(HashSet::new()),
            &CancellationToken::new(),
            8,
        )
        .await?;
        assert_eq!(batch.removed_records, 1);
        assert_eq!(batch.removed_stages, 1);
        assert!(!stage.exists());
        assert!(store.upload_session(key).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn running_expiry_preserves_a_reused_stage_inode() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let store = temporary_store();
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("reused.bin");
        let stage = upload_temp_path(&target, upload_id)?;
        let owner = OwnerId::persistent("reused-owner").into_bytes();
        let original = create_stage(&rooted_fs, &stage, b"part").await;
        let metadata = original.metadata().await?;
        let session = stored_session(
            &rooted_fs,
            owner,
            upload_id,
            &target,
            &stage,
            StoredUploadState::Running,
            Some(StoredFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
        );
        let key = session.key;
        store.save_upload_session(session, EXPIRED_TTL).await?;
        std::fs::remove_file(&stage)?;
        let replacement = create_stage(&rooted_fs, &stage, b"replacement").await;
        drop(replacement);
        drop(original);
        expire().await;

        let batch = cleanup_expired_upload_sessions_batch(
            &rooted_fs,
            &store,
            &Mutex::new(HashSet::new()),
            &CancellationToken::new(),
            8,
        )
        .await?;
        assert_eq!(batch.removed_records, 1);
        assert_eq!(batch.removed_stages, 0);
        assert_eq!(std::fs::read(&stage)?, b"replacement");
        assert!(store.upload_session(key).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_expiry_preserves_commit_started_and_retires_unknown() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let store = temporary_store();
        let mut keys = Vec::new();
        for (index, state) in [StoredUploadState::CommitStarted, StoredUploadState::Unknown]
            .into_iter()
            .enumerate()
        {
            let upload_id = Uuid::new_v4();
            let target = temp.path().join(format!("ambiguous-{index}.bin"));
            let stage = upload_temp_path(&target, upload_id)?;
            std::fs::write(&target, b"published")?;
            std::fs::write(&stage, b"stage")?;
            let session = stored_session(
                &rooted_fs,
                [index as u8 + 1; 32],
                upload_id,
                &target,
                &stage,
                state,
                None,
            );
            keys.push((session.key, target, stage));
            store.save_upload_session(session, EXPIRED_TTL).await?;
        }
        expire().await;

        let batch = cleanup_expired_upload_sessions_batch(
            &rooted_fs,
            &store,
            &Mutex::new(HashSet::new()),
            &CancellationToken::new(),
            8,
        )
        .await?;
        assert_eq!(batch.removed_records, 1);
        assert_eq!(batch.removed_stages, 0);
        for (index, (key, target, stage)) in keys.into_iter().enumerate() {
            assert_eq!(store.upload_session(key).await?.is_some(), index == 0);
            assert_eq!(std::fs::read(target)?, b"published");
            assert_eq!(std::fs::read(stage)?, b"stage");
        }
        Ok(())
    }

    #[tokio::test]
    async fn terminal_expiry_never_opens_an_untrusted_stage_parent() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = assert_fs::TempDir::new()?;
        let root = temp.path().join("root");
        std::fs::create_dir(&root)?;
        let rooted_fs = RootedFs::new(&root)?;
        let store = temporary_store();
        let upload_id = Uuid::new_v4();
        let target = root.join("untrusted-parent").join("terminal.bin");
        let stage = upload_temp_path(&target, upload_id)?;
        let session = stored_session(
            &rooted_fs,
            [7; 32],
            upload_id,
            &target,
            &stage,
            StoredUploadState::Committed,
            None,
        );
        let key = session.key;
        store.save_upload_session(session, EXPIRED_TTL).await?;
        symlink(temp.path(), root.join("untrusted-parent"))?;
        expire().await;

        let batch = cleanup_expired_upload_sessions_batch(
            &rooted_fs,
            &store,
            &Mutex::new(HashSet::new()),
            &CancellationToken::new(),
            8,
        )
        .await?;
        assert_eq!(batch.removed_records, 1);
        assert!(store.upload_session(key).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn untrusted_stored_paths_never_escape_the_root() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let root = temp.path().join("root");
        std::fs::create_dir(&root)?;
        let outside = temp.path().join("outside.part");
        std::fs::write(&outside, b"outside")?;
        let rooted_fs = RootedFs::new(&root)?;
        let store = temporary_store();
        let session = StoredUploadSession {
            key: UploadSessionKey {
                owner: [1; 32],
                id: *Uuid::new_v4().as_bytes(),
            },
            target_path: PathBuf::from("../outside.bin"),
            stage_path: PathBuf::from("../outside.part"),
            upload_length: 8,
            durable_offset: 4,
            state: StoredUploadState::Running,
            stage_identity: None,
            target_revision: None,
        };

        let mut batch = UploadSessionCleanupBatch::default();
        assert!(
            cleanup_expired_upload_session(
                &rooted_fs,
                &store,
                &Mutex::new(HashSet::new()),
                &CancellationToken::new(),
                &session,
                &mut batch,
            )
            .await?
            .is_none()
        );
        assert_eq!(std::fs::read(outside)?, b"outside");
        Ok(())
    }

    #[test]
    fn orphan_scan_cannot_bypass_a_sqlite_cleanup_protection() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("protected.bin");
        let stage = upload_temp_path(&target, upload_id)?;
        std::fs::write(&stage, b"do-not-delete")?;
        let stage_key = rooted_fs.entry_key_blocking(&stage)?;
        let mut state = MaintenanceScanState::new(temp.path().to_path_buf(), Duration::ZERO);
        state.protected_upload_entries.insert(stage_key);

        let (_, removed, _, complete, _) = collect_stale_internal_files_batch(
            &rooted_fs,
            state,
            &Mutex::new(HashSet::new()),
            MaintenanceBatchOptions {
                now: SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
                upload_ttl: UPLOAD_SESSION_TTL,
                budget: MaintenanceBudget {
                    max_entries: 16,
                    max_duration: Duration::from_secs(1),
                },
            },
            &CancellationToken::new(),
            |_| unreachable!("a protected upload stage is not delete trash"),
        );
        assert!(complete);
        assert!(removed.is_empty());
        assert_eq!(std::fs::read(stage)?, b"do-not-delete");
        Ok(())
    }

    #[tokio::test]
    async fn tracked_replacement_trash_is_never_treated_as_an_orphan() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let store = temporary_store();
        let job_id = Uuid::new_v4();
        let target = temp.path().join("tracked-delete.bin");
        let trash = rooted_fs.trash_path_for_id(&target, job_id)?;
        std::fs::write(&target, b"original")?;
        let source = std::fs::metadata(&target)?;
        std::fs::rename(&target, &trash)?;
        let original = std::fs::File::open(&trash)?;
        let job = StoredPurgeJob {
            key: PurgeJobKey {
                owner: [3; 32],
                id: *job_id.as_bytes(),
            },
            target_path: rooted_fs.state_relative_path(&target)?,
            trash_path: rooted_fs.state_relative_path(&trash)?,
            source_identity: StoredFileIdentity {
                device: source.dev(),
                inode: source.ino(),
            },
            trash_revision: None,
            is_directory: false,
            state: StoredPurgeState::Prepared,
            attempts: 0,
        };
        assert_eq!(store.prepare_purge_job(job).await?, StorePurgeJob::Inserted);

        std::fs::remove_file(&trash)?;
        std::fs::write(&trash, b"replacement")?;
        drop(original);
        let mut state =
            MaintenanceScanState::new(temp.path().to_path_buf(), Duration::from_secs(60 * 60));
        load_tracked_purge_snapshot(&rooted_fs, &store, &mut state).await;
        assert!(state.purge_job_snapshot_complete);
        assert!(!state.skip_untracked_trash_cleanup);
        assert_eq!(state.protected_trash_entries.len(), 1);

        let (_, _, scheduled, complete, _) = collect_stale_internal_files_batch(
            &rooted_fs,
            state,
            &Mutex::new(HashSet::new()),
            MaintenanceBatchOptions {
                now: SystemTime::now() + Duration::from_secs(2 * 60 * 60),
                upload_ttl: UPLOAD_SESSION_TTL,
                budget: MaintenanceBudget {
                    max_entries: 16,
                    max_duration: Duration::from_secs(1),
                },
            },
            &CancellationToken::new(),
            |_| unreachable!("tracked trash must be owned only by the durable purge worker"),
        );
        assert!(complete);
        assert!(scheduled.is_empty());
        assert_eq!(std::fs::read(trash)?, b"replacement");
        Ok(())
    }

    #[test]
    fn untracked_old_trash_is_still_scheduled() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let target = temp.path().join("orphan.bin");
        let trash = rooted_fs.trash_path_for_id(&target, Uuid::new_v4())?;
        std::fs::write(&trash, b"orphan")?;
        let state =
            MaintenanceScanState::new(temp.path().to_path_buf(), Duration::from_secs(60 * 60));

        let (_, _, scheduled, complete, _) = collect_stale_internal_files_batch(
            &rooted_fs,
            state,
            &Mutex::new(HashSet::new()),
            MaintenanceBatchOptions {
                now: SystemTime::now() + Duration::from_secs(2 * 60 * 60),
                upload_ttl: UPLOAD_SESSION_TTL,
                budget: MaintenanceBudget {
                    max_entries: 16,
                    max_duration: Duration::from_secs(1),
                },
            },
            &CancellationToken::new(),
            |entry| {
                entry.purge_all_blocking().unwrap();
                Ok(())
            },
        );
        assert!(complete);
        assert_eq!(scheduled, vec![trash.clone()]);
        assert!(!trash.exists());
        Ok(())
    }

    #[test]
    fn newly_renamed_old_source_observes_the_trash_grace_period() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let source = temp.path().join("old-source.bin");
        let trash = rooted_fs.trash_path_for_id(&source, Uuid::new_v4())?;
        std::fs::write(&source, b"old source")?;
        let old_modified = SystemTime::now() - Duration::from_secs(3 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&source)?
            .set_times(std::fs::FileTimes::new().set_modified(old_modified))?;
        std::fs::rename(&source, &trash)?;
        let scan_now = SystemTime::now() + Duration::from_secs(1);
        let metadata = std::fs::metadata(&trash)?;
        assert!(
            maintenance_entry_age(&metadata, scan_now, false) >= Duration::from_secs(60 * 60),
            "mtime alone would incorrectly classify the newly renamed old source as orphan trash"
        );
        assert!(
            maintenance_entry_age(&metadata, scan_now, true) < Duration::from_secs(60 * 60),
            "trash age must include the rename-updated ctime"
        );
        let state =
            MaintenanceScanState::new(temp.path().to_path_buf(), Duration::from_secs(60 * 60));

        let (_, _, scheduled, complete, _) = collect_stale_internal_files_batch(
            &rooted_fs,
            state,
            &Mutex::new(HashSet::new()),
            MaintenanceBatchOptions {
                now: scan_now,
                upload_ttl: UPLOAD_SESSION_TTL,
                budget: MaintenanceBudget {
                    max_entries: 16,
                    max_duration: Duration::from_secs(1),
                },
            },
            &CancellationToken::new(),
            |_| unreachable!("newly renamed trash is still inside its grace period"),
        );
        assert!(complete);
        assert!(scheduled.is_empty());
        assert!(trash.exists());
        Ok(())
    }

    #[tokio::test]
    async fn invalid_purge_snapshot_fails_closed_without_blocking_stage_cleanup() -> Result<()> {
        let temp = assert_fs::TempDir::new()?;
        let rooted_fs = RootedFs::new(temp.path())?;
        let store = temporary_store();
        let job_id = Uuid::new_v4();
        assert_eq!(
            store
                .prepare_purge_job(StoredPurgeJob {
                    key: PurgeJobKey {
                        owner: [4; 32],
                        id: *job_id.as_bytes(),
                    },
                    target_path: PathBuf::from("invalid-target.bin"),
                    trash_path: PathBuf::from("../outside.trash"),
                    source_identity: StoredFileIdentity {
                        device: 1,
                        inode: 1,
                    },
                    trash_revision: None,
                    is_directory: false,
                    state: StoredPurgeState::Prepared,
                    attempts: 0,
                })
                .await?,
            StorePurgeJob::Inserted
        );

        let target = temp.path().join("untracked.bin");
        let trash = rooted_fs.trash_path_for_id(&target, Uuid::new_v4())?;
        std::fs::write(&trash, b"preserve")?;
        let stage_target = temp.path().join("stale-stage.bin");
        let stage = upload_temp_path(&stage_target, Uuid::new_v4())?;
        std::fs::write(&stage, b"stale")?;
        let mut state =
            MaintenanceScanState::new(temp.path().to_path_buf(), Duration::from_secs(60 * 60));
        load_tracked_purge_snapshot(&rooted_fs, &store, &mut state).await;
        assert!(state.purge_job_snapshot_complete);
        assert!(state.skip_untracked_trash_cleanup);

        let (_, removed, scheduled, complete, _) = collect_stale_internal_files_batch(
            &rooted_fs,
            state,
            &Mutex::new(HashSet::new()),
            MaintenanceBatchOptions {
                now: SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
                upload_ttl: UPLOAD_SESSION_TTL,
                budget: MaintenanceBudget {
                    max_entries: 32,
                    max_duration: Duration::from_secs(1),
                },
            },
            &CancellationToken::new(),
            |_| unreachable!("a failed purge snapshot must disable orphan-trash scheduling"),
        );
        assert!(complete);
        assert!(scheduled.is_empty());
        assert!(trash.exists());
        assert!(removed.contains(&stage));
        assert!(!stage.exists());
        Ok(())
    }
}
