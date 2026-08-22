use super::super::ServerLifecycle;
use super::*;
use crate::{Args, auth::AuthConfig};
use std::{os::unix::fs::PermissionsExt, sync::Arc};

const TEST_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

fn server(root: &Path) -> (Arc<Server>, assert_fs::TempDir) {
    let state_dir = assert_fs::TempDir::new().unwrap();
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Arc::new(
        Server::init_with_lifecycle(
            Args {
                serve_path: root.to_path_buf(),
                state_dir: Some(state_dir.path().to_path_buf()),
                auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
                ..Args::default()
            },
            ServerLifecycle::new(),
        )
        .unwrap(),
    );
    (server, state_dir)
}

async fn prepared(server: &Server, target: &Path) -> (PreparedPurge, StoredPurgeJob) {
    let PreparePurge::Prepared(prepared) = server.prepare_purge("owner", target).await.unwrap()
    else {
        panic!("test purge repository unexpectedly full");
    };
    let job = server
        .state
        .state_store
        .purge_job(prepared.key)
        .await
        .unwrap()
        .unwrap();
    (prepared, job)
}

#[tokio::test]
async fn reconciliation_discards_an_intent_when_rename_never_started() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("kept.txt");
    std::fs::write(&target, "kept").unwrap();
    let (prepared, job) = prepared(&server, &target).await;

    server.reconcile_prepared_purge_job(&job).await.unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"kept");
    assert!(
        server
            .state
            .state_store
            .purge_job(prepared.key)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reconciliation_promotes_an_intent_after_the_checked_rename() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("removed.txt");
    std::fs::write(&target, "removed").unwrap();
    let (prepared, job) = prepared(&server, &target).await;
    let trash = server
        .content
        .rooted_fs
        .trash_path_for_id(&target, prepared.trash_id)
        .unwrap();
    drop(
        server
            .content
            .rooted_fs
            .move_to_trash_with_expected_identity(
                &target,
                prepared.trash_id,
                prepared.source_identity,
            )
            .await
            .unwrap(),
    );

    server.reconcile_prepared_purge_job(&job).await.unwrap();

    let ready = server
        .state
        .state_store
        .purge_job(prepared.key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready.state, StoredPurgeState::Ready);
    let claimed = server
        .state
        .state_store
        .claim_due_purge_job()
        .await
        .unwrap()
        .unwrap();
    let work = server.open_purge_work(claimed).await.unwrap().unwrap();
    assert!(matches!(
        server.process_purge_work(work).await,
        PurgeWorkResult::Complete
    ));
    assert!(!trash.exists());
    assert!(
        server
            .state
            .state_store
            .purge_job(prepared.key)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reconciliation_quarantines_unrelated_trash_and_releases_ambiguous_intent() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("ambiguous.txt");
    std::fs::write(&target, "original").unwrap();
    let (prepared, job) = prepared(&server, &target).await;
    let trash = server
        .content
        .rooted_fs
        .trash_path_for_id(&target, prepared.trash_id)
        .unwrap();

    std::fs::remove_file(&target).unwrap();
    std::fs::write(&target, "replacement").unwrap();
    std::fs::write(&trash, "unrelated trash occupant").unwrap();

    server.reconcile_prepared_purge_job(&job).await.unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
    assert!(!trash.exists());
    assert!(
        server
            .state
            .state_store
            .purge_job(prepared.key)
            .await
            .unwrap()
            .is_none(),
        "an isolated ambiguous entry must not consume purge capacity forever"
    );
    let quarantined = std::fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with(".dufs-quarantine-") && name.ends_with(".hold")
            })
        })
        .expect("identity-ambiguous trash must be retained in quarantine");
    assert_eq!(
        std::fs::read(quarantined.path()).unwrap(),
        b"unrelated trash occupant"
    );
}

#[tokio::test]
async fn claimed_identity_mismatch_is_quarantined_instead_of_retried_or_deleted() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("claimed-ambiguous.txt");
    std::fs::write(&target, "original").unwrap();
    let (prepared, _) = prepared(&server, &target).await;
    let trash = server
        .content
        .rooted_fs
        .trash_path_for_id(&target, prepared.trash_id)
        .unwrap();
    drop(
        server
            .content
            .rooted_fs
            .move_to_trash_with_expected_identity(
                &target,
                prepared.trash_id,
                prepared.source_identity,
            )
            .await
            .unwrap(),
    );
    assert!(
        server
            .state
            .state_store
            .mark_purge_job_ready(prepared.key)
            .await
            .unwrap()
    );
    let claimed = server
        .state
        .state_store
        .claim_due_purge_job()
        .await
        .unwrap()
        .unwrap();

    std::fs::remove_file(&trash).unwrap();
    std::fs::write(&trash, "external replacement").unwrap();

    assert!(server.open_purge_work(claimed).await.unwrap().is_none());
    assert!(!trash.exists());
    assert!(
        server
            .state
            .state_store
            .purge_job(prepared.key)
            .await
            .unwrap()
            .is_none()
    );
    let quarantined = std::fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with(".dufs-quarantine-") && name.ends_with(".hold")
            })
        })
        .expect("a replacement at a claimed trash name must be quarantined");
    assert_eq!(
        std::fs::read(quarantined.path()).unwrap(),
        b"external replacement"
    );
}

#[tokio::test]
async fn identity_change_during_claimed_purge_quarantines_the_replacement_root() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("claimed-directory");
    std::fs::create_dir(&target).unwrap();
    let (prepared, _) = prepared(&server, &target).await;
    let trash = server
        .content
        .rooted_fs
        .trash_path_for_id(&target, prepared.trash_id)
        .unwrap();
    drop(
        server
            .content
            .rooted_fs
            .move_to_trash_with_expected_identity(
                &target,
                prepared.trash_id,
                prepared.source_identity,
            )
            .await
            .unwrap(),
    );
    assert!(
        server
            .state
            .state_store
            .mark_purge_job_ready(prepared.key)
            .await
            .unwrap()
    );
    let claimed = server
        .state
        .state_store
        .claim_due_purge_job()
        .await
        .unwrap()
        .unwrap();
    let work = server.open_purge_work(claimed).await.unwrap().unwrap();

    std::fs::remove_dir(&trash).unwrap();
    std::fs::create_dir(&trash).unwrap();
    std::fs::write(trash.join("preserve.txt"), "external replacement").unwrap();

    assert!(matches!(
        server.process_purge_work(work).await,
        PurgeWorkResult::Complete
    ));
    assert!(!trash.exists());
    assert!(
        server
            .state
            .state_store
            .purge_job(prepared.key)
            .await
            .unwrap()
            .is_none()
    );
    let quarantined = std::fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with(".dufs-quarantine-") && name.ends_with(".hold")
            })
        })
        .expect("a replacement root discovered during purge must be quarantined");
    assert_eq!(
        std::fs::read(quarantined.path().join("preserve.txt")).unwrap(),
        b"external replacement"
    );
}

#[tokio::test]
async fn a_locally_retained_claim_is_reloaded_before_retrying() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("claimed-retry.txt");
    std::fs::write(&target, "removed").unwrap();
    let (prepared, _) = prepared(&server, &target).await;
    let trash = server
        .content
        .rooted_fs
        .trash_path_for_id(&target, prepared.trash_id)
        .unwrap();
    drop(
        server
            .content
            .rooted_fs
            .move_to_trash_with_expected_identity(
                &target,
                prepared.trash_id,
                prepared.source_identity,
            )
            .await
            .unwrap(),
    );
    assert!(
        server
            .state
            .state_store
            .mark_purge_job_ready(prepared.key)
            .await
            .unwrap()
    );
    let claimed = server
        .state
        .state_store
        .claim_due_purge_job()
        .await
        .unwrap()
        .unwrap();

    let work = server
        .retry_claimed_purge_work(claimed)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        server.process_purge_work(work).await,
        PurgeWorkResult::Complete
    ));
    assert!(!trash.exists());
    assert!(
        server
            .state
            .state_store
            .purge_job(prepared.key)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_locally_retained_claim_is_dropped_after_a_lost_retry_reply() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("claimed-already-retried.txt");
    std::fs::write(&target, "kept in trash").unwrap();
    let (prepared, _) = prepared(&server, &target).await;
    let trash = server
        .content
        .rooted_fs
        .trash_path_for_id(&target, prepared.trash_id)
        .unwrap();
    drop(
        server
            .content
            .rooted_fs
            .move_to_trash_with_expected_identity(
                &target,
                prepared.trash_id,
                prepared.source_identity,
            )
            .await
            .unwrap(),
    );
    assert!(
        server
            .state
            .state_store
            .mark_purge_job_ready(prepared.key)
            .await
            .unwrap()
    );
    let claimed = server
        .state
        .state_store
        .claim_due_purge_job()
        .await
        .unwrap()
        .unwrap();
    assert!(
        server
            .state
            .state_store
            .retry_purge_job(prepared.key, Duration::ZERO)
            .await
            .unwrap()
    );

    assert!(
        server
            .retry_claimed_purge_work(claimed)
            .await
            .unwrap()
            .is_none()
    );
    assert!(trash.exists());
    assert_eq!(
        server
            .state
            .state_store
            .purge_job(prepared.key)
            .await
            .unwrap()
            .unwrap()
            .state,
        StoredPurgeState::Ready
    );
}

#[tokio::test]
async fn periodic_reconciliation_recovers_a_post_rename_prepared_intent() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("post-rename.txt");
    std::fs::write(&target, "removed").unwrap();
    let (prepared, _) = prepared(&server, &target).await;
    drop(
        server
            .content
            .rooted_fs
            .move_to_trash_with_expected_identity(
                &target,
                prepared.trash_id,
                prepared.source_identity,
            )
            .await
            .unwrap(),
    );

    let reconciler_server = server.clone();
    let reconciler = tokio::spawn(async move {
        reconciler_server.run_prepared_purge_reconciler().await;
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if server
                .state
                .state_store
                .purge_job(prepared.key)
                .await
                .unwrap()
                .is_some_and(|job| job.state == StoredPurgeState::Ready)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("periodic reconciliation did not promote the prepared intent");

    server.lifecycle.shutdown.cancel();
    reconciler.await.unwrap();
}

#[tokio::test]
async fn reconciliation_waits_for_the_live_delete_path_lease() {
    let temp = assert_fs::TempDir::new().unwrap();
    let (server, _state_dir) = server(temp.path());
    let target = temp.path().join("racing.txt");
    std::fs::write(&target, "removed").unwrap();
    let lease = server.content.path_coordinator.acquire([&target]).await;
    let (prepared, job) = prepared(&server, &target).await;
    let task_server = server.clone();
    let mut reconciliation =
        tokio::spawn(async move { task_server.reconcile_prepared_purge_job(&job).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut reconciliation)
            .await
            .is_err(),
        "reconciliation bypassed the live DELETE lease"
    );

    drop(
        server
            .content
            .rooted_fs
            .move_to_trash_with_expected_identity(
                &target,
                prepared.trash_id,
                prepared.source_identity,
            )
            .await
            .unwrap(),
    );
    assert!(
        server
            .state
            .state_store
            .mark_purge_job_ready(prepared.key)
            .await
            .unwrap()
    );
    drop(lease);
    reconciliation.await.unwrap().unwrap();

    assert_eq!(
        server
            .state
            .state_store
            .purge_job(prepared.key)
            .await
            .unwrap()
            .unwrap()
            .state,
        StoredPurgeState::Ready
    );
}
