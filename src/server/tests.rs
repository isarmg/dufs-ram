use super::{purge::PreparePurge, *};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::oneshot;

const TEST_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

fn private_state_dir() -> assert_fs::TempDir {
    let state_dir = assert_fs::TempDir::new().unwrap();
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    state_dir
}

fn authenticated_args(serve_path: PathBuf, state_dir: &Path) -> Args {
    Args {
        serve_path,
        state_dir: Some(state_dir.to_path_buf()),
        auth: crate::auth::AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
        ..Args::default()
    }
}

#[tokio::test]
async fn shutdown_gate_drains_admitted_requests_and_rejects_late_entries() {
    let lifecycle = ServerLifecycle::new();
    let active_request = lifecycle
        .enter_request()
        .await
        .expect("a running server must admit requests");
    lifecycle.shutdown.cancel();

    let gate = lifecycle.request_gate.clone();
    let mut drain = tokio::spawn(async move {
        let _exclusive = gate.write_owned().await;
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut drain)
            .await
            .is_err(),
        "shutdown crossed the request drain while a request was active"
    );

    drop(active_request);
    tokio::time::timeout(Duration::from_secs(1), drain)
        .await
        .expect("shutdown did not acquire the request drain")
        .unwrap();
    assert!(
        lifecycle.enter_request().await.is_none(),
        "a request entered after shutdown cancellation"
    );
}

#[test]
fn server_init_rejects_a_non_directory_root() {
    let temp = assert_fs::TempDir::new().unwrap();
    let file = temp.path().join("shared.txt");
    std::fs::write(&file, "contents").unwrap();
    let args = Args {
        serve_path: file,
        ..Args::default()
    };
    let error = match Server::init_with_lifecycle(args, ServerLifecycle::new()) {
        Ok(_) => panic!("a regular file must not be accepted as the shared root"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("directory"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn server_init_rejects_unvalidated_runtime_limits() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let mut args = authenticated_args(temp.path().to_path_buf(), state_dir.path());
    args.max_concurrent_searches = 0;
    let error = match Server::init_with_lifecycle(args, ServerLifecycle::new()) {
        Ok(_) => panic!("an invalid library configuration was accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("max-concurrent-searches"));
}

#[test]
fn servers_from_the_same_auth_config_have_isolated_sessions() {
    let temp = assert_fs::TempDir::new().unwrap();
    let first_root = temp.path().join("first");
    let second_root = temp.path().join("second");
    std::fs::create_dir(&first_root).unwrap();
    std::fs::create_dir(&second_root).unwrap();
    let first_state_dir = private_state_dir();
    let second_state_dir = private_state_dir();
    let auth = crate::auth::AuthConfig::new(&[TEST_ACCOUNT]).unwrap();
    let make_server = |auth, serve_path, state_dir: &Path| {
        Server::init_with_lifecycle(
            Args {
                serve_path,
                state_dir: Some(state_dir.to_path_buf()),
                auth,
                ..Args::default()
            },
            ServerLifecycle::new(),
        )
        .unwrap()
    };
    let first = make_server(auth.clone(), first_root, first_state_dir.path());
    let second = make_server(auth, second_root, second_state_dir.path());

    let created = first
        .content
        .auth
        .login("user", "test-password", None)
        .unwrap()
        .unwrap();
    assert!(first.content.auth.authenticate(&created.token).is_some());
    assert!(second.content.auth.authenticate(&created.token).is_none());
}

#[tokio::test]
async fn tracked_mutation_keeps_its_path_lease_after_waiter_cancellation() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let args = authenticated_args(temp.path().to_path_buf(), state_dir.path());
    let lifecycle = ServerLifecycle::new();
    let commit_tasks = lifecycle.commit_tasks.clone();
    let server = Arc::new(Server::init_with_lifecycle(args, lifecycle).unwrap());
    let target = temp.path().join("directory/file.txt");
    let ancestor = temp.path().join("directory");
    let path_lease = server.content.path_coordinator.acquire([&target]).await;
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let waiter = {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .run_commit(async move {
                    let _path_lease = path_lease;
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                    Ok(())
                })
                .await
        })
    };
    started_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());

    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            server.content.path_coordinator.acquire([&ancestor]),
        )
        .await
        .is_err(),
        "cancelling the HTTP waiter released a live mutation lease"
    );

    release_tx.send(()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        server.content.path_coordinator.acquire([&ancestor]),
    )
    .await
    .expect("tracked mutation did not release its lease after completion");
    commit_tasks.close();
    tokio::time::timeout(Duration::from_secs(1), commit_tasks.wait())
        .await
        .expect("tracked mutation task did not finish");
}

#[tokio::test]
async fn durable_purge_intents_are_not_limited_by_the_wakeup_channel() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let server = Server::init_with_lifecycle(
        authenticated_args(temp.path().to_path_buf(), state_dir.path()),
        ServerLifecycle::new(),
    )
    .unwrap();

    const JOBS: usize = 96;
    for index in 0..JOBS {
        let path = temp.path().join(format!("prepared-{index:02}.txt"));
        std::fs::write(&path, "content").unwrap();
        assert!(matches!(
            server.prepare_purge("user", &path).await.unwrap(),
            PreparePurge::Prepared(_)
        ));
    }
    assert_eq!(
        server
            .state
            .state_store
            .prepared_purge_jobs(JOBS + 1)
            .await
            .unwrap()
            .len(),
        JOBS
    );
}

#[test]
fn purge_retry_backoff_is_exponential_and_capped() {
    assert_eq!(purge_retry_delay(1), DELETE_PURGE_RETRY_BASE);
    assert_eq!(
        purge_retry_delay(2),
        DELETE_PURGE_RETRY_BASE.saturating_mul(2)
    );
    assert_eq!(
        purge_retry_delay(3),
        DELETE_PURGE_RETRY_BASE.saturating_mul(4)
    );
    assert_eq!(purge_retry_delay(u32::MAX), DELETE_PURGE_RETRY_MAX);
}

#[tokio::test]
async fn changed_purge_identity_is_preserved_without_blocking_later_jobs() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let lifecycle = ServerLifecycle::new();
    let shutdown = lifecycle.shutdown.clone();
    let server = Arc::new(
        Server::init_with_lifecycle(
            authenticated_args(temp.path().to_path_buf(), state_dir.path()),
            lifecycle,
        )
        .unwrap(),
    );
    let receiver = server
        .state
        .purge_receiver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap();

    let bad_path = temp.path().join("bad");
    std::fs::create_dir(&bad_path).unwrap();
    let PreparePurge::Prepared(bad_prepared) =
        server.prepare_purge("user", &bad_path).await.unwrap()
    else {
        panic!("durable purge store unexpectedly full");
    };
    let bad_trash = server
        .content
        .rooted_fs
        .trash_path_for_id(&bad_path, bad_prepared.trash_id)
        .unwrap();
    let bad = server
        .content
        .rooted_fs
        .move_to_trash_with_expected_identity(
            &bad_path,
            bad_prepared.trash_id,
            bad_prepared.source_identity,
        )
        .await
        .unwrap();
    bad.replace_directory_with_file_for_test().unwrap();
    std::fs::write(&bad_trash, "unrelated replacement").unwrap();
    let bad_replacement = std::fs::symlink_metadata(&bad_trash).unwrap();
    assert!(
        server
            .state
            .state_store
            .mark_purge_job_ready(bad_prepared.key, bad.trash_revision())
            .await
            .unwrap()
    );

    let good_path = temp.path().join("good");
    std::fs::write(&good_path, "content").unwrap();
    let PreparePurge::Prepared(good_prepared) =
        server.prepare_purge("user", &good_path).await.unwrap()
    else {
        panic!("durable purge store unexpectedly full");
    };
    let good_trash = server
        .content
        .rooted_fs
        .trash_path_for_id(&good_path, good_prepared.trash_id)
        .unwrap();
    let good = server
        .content
        .rooted_fs
        .move_to_trash_with_expected_identity(
            &good_path,
            good_prepared.trash_id,
            good_prepared.source_identity,
        )
        .await
        .unwrap();
    assert!(
        server
            .state
            .state_store
            .mark_purge_job_ready(good_prepared.key, good.trash_revision())
            .await
            .unwrap()
    );
    drop(good);
    server.notify_purge_worker();

    let worker = tokio::spawn(server.clone().run_purge_worker(receiver));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !good_trash.exists()
                && server
                    .state
                    .state_store
                    .purge_job(good_prepared.key)
                    .await
                    .unwrap()
                    .is_none()
                && server
                    .state
                    .state_store
                    .purge_job(bad_prepared.key)
                    .await
                    .unwrap()
                    .is_none()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the healthy purge job remained behind a failing queue entry");

    assert!(
        !bad_trash.exists(),
        "the ambiguous replacement must leave the automatic trash namespace"
    );
    let quarantined = std::fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with(".dufs-quarantine-") && name.ends_with(".hold")
            })
        })
        .expect("the identity-ambiguous replacement must be quarantined");
    let quarantined_metadata = quarantined.metadata().unwrap();
    assert_eq!(quarantined_metadata.dev(), bad_replacement.dev());
    assert_eq!(quarantined_metadata.ino(), bad_replacement.ino());
    assert_eq!(
        std::fs::read_to_string(quarantined.path()).unwrap(),
        "unrelated replacement"
    );

    shutdown.cancel();
    worker.await.unwrap();
}

#[tokio::test]
async fn non_upload_mutations_wait_for_bounded_admission() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let lifecycle = ServerLifecycle::new();
    let commit_tasks = lifecycle.commit_tasks.clone();
    let server = Arc::new(
        Server::init_with_lifecycle(
            authenticated_args(temp.path().to_path_buf(), state_dir.path()),
            lifecycle,
        )
        .unwrap(),
    );
    let mut permits = Vec::new();
    for _ in 0..NON_UPLOAD_MUTATION_CAPACITY {
        permits.push(
            server
                .admission
                .mutation_slots
                .clone()
                .acquire_owned()
                .await
                .unwrap(),
        );
    }
    let (started_tx, mut started_rx) = oneshot::channel();
    let waiter = {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .run_commit(async move {
                    let _ = started_tx.send(());
                    Ok(())
                })
                .await
        })
    };
    tokio::task::yield_now().await;
    assert!(
        matches!(
            started_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "a mutation started without an admission permit"
    );

    drop(permits.pop());
    tokio::time::timeout(Duration::from_secs(1), &mut started_rx)
        .await
        .expect("the admitted mutation did not start")
        .unwrap();
    waiter.await.unwrap().unwrap();
    drop(permits);
    commit_tasks.close();
    commit_tasks.wait().await;
}
