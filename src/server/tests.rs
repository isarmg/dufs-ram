use super::{purge::PreparePurge, *};
use futures_util::poll;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    task::Poll,
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
async fn pre_mutation_upload_timeout_retains_tracked_work_until_actor_reply() {
    let root = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let mut args = authenticated_args(root.path().to_path_buf(), state_dir.path());
    args.max_connections = 2;
    args.max_concurrent_uploads = 1;
    args.min_free_space = 0;
    let server = Arc::new(Server::init_with_lifecycle(args, ServerLifecycle::new()).unwrap());
    let release_actor = server.state.state_store.block_actor_for_test().unwrap();
    let target = root.path().join("pre-mutation-timeout.bin");
    let upload_id = uuid::Uuid::new_v4();
    let stage = internal_names::upload_temp_path(&target, upload_id).unwrap();
    let path_lease = server
        .acquire_request_path_lease([&target])
        .await
        .expect("test upload could not acquire path admission");
    let upload_permit = server
        .admission
        .upload_slots
        .clone()
        .try_acquire_owned()
        .expect("test upload could not acquire upload admission");
    let mutation = router::MutationProgress::default();
    let task_mutation = mutation.clone();
    let body_polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task_body_polls = body_polls.clone();
    let task_stage = stage.clone();
    let state_store = server.state.state_store.clone();
    let (actor_waiting_sender, actor_waiting_receiver) = oneshot::channel();
    let task = server.lifecycle.commit_tasks.spawn(async move {
        let _upload_permit = upload_permit;
        let _path_lease = path_lease;
        let mut lookup = Box::pin(state_store.upload_session(state_store::UploadSessionKey {
            owner: [7; 32],
            id: *upload_id.as_bytes(),
        }));
        assert!(matches!(poll!(lookup.as_mut()), Poll::Pending));
        let _ = actor_waiting_sender.send(());
        let _ = lookup.await?;
        // These model the first guarded namespace/body actions after the
        // blocked read-only preparation. Closing MutationProgress must make
        // both unreachable when the actor command eventually returns.
        if task_mutation.begin_upload_mutation() {
            task_body_polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::fs::create_dir_all(task_stage.parent().unwrap())?;
            std::fs::write(&task_stage, b"unexpected staged body")?;
        }
        Ok(Response::default())
    });
    tokio::time::timeout(Duration::from_secs(1), actor_waiting_receiver)
        .await
        .expect("tracked upload did not dispatch its actor command")
        .expect("tracked upload exited before waiting for the actor");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);

    let response = tokio::time::timeout(
        Duration::from_secs(1),
        Server::await_tracked_upload_task(deadline, upload_id, 7, None, mutation.clone(), task),
    )
    .await
    .expect("pre-mutation upload timeout did not return promptly")
    .unwrap();

    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(response.headers()["x-dufs-operation-state"], "not-started");
    assert!(!mutation.outcome_can_be_unknown());
    assert_eq!(server.admission.upload_slots.available_permits(), 0);
    assert_eq!(server.admission.path_wait_slots.available_permits(), 0);
    assert_eq!(server.lifecycle.commit_tasks.len(), 1);
    assert_eq!(
        body_polls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the timed-out task polled the request body"
    );
    assert!(!stage.exists(), "the timed-out task created a staging file");

    let mut same_path = Box::pin(server.content.path_coordinator.acquire([&target]));
    assert!(
        matches!(poll!(same_path.as_mut()), Poll::Pending),
        "the timed-out task released its path lease before actor work finished"
    );

    release_actor.send(()).unwrap();
    let acquired = tokio::time::timeout(Duration::from_secs(1), same_path)
        .await
        .expect("tracked upload did not release its path lease after the actor replied");
    drop(acquired);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if server.lifecycle.commit_tasks.is_empty()
                && server.admission.upload_slots.available_permits() == 1
                && server.admission.path_wait_slots.available_permits() == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tracked upload resources were not released after actor completion");

    assert_eq!(
        body_polls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the cancelled mutation boundary allowed a later body poll"
    );
    assert!(!stage.exists());
}

#[test]
fn path_wait_capacity_reserves_a_connection_without_disabling_single_connection_servers() {
    assert_eq!(path_wait_capacity(1), 1);
    assert_eq!(path_wait_capacity(2), 1);
    assert_eq!(path_wait_capacity(11), 10);
    assert_eq!(path_wait_capacity(65), PATH_WAIT_CAPACITY_LIMIT);
    assert_eq!(path_wait_capacity(256), PATH_WAIT_CAPACITY_LIMIT);
}

#[tokio::test]
async fn single_connection_server_keeps_path_coordinated_requests_usable() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let mut args = authenticated_args(temp.path().to_path_buf(), state_dir.path());
    args.max_connections = 1;
    let server = Server::init_with_lifecycle(args, ServerLifecycle::new()).unwrap();

    assert_eq!(server.admission.path_wait_slots.available_permits(), 1);
    let lease = server
        .acquire_request_path_lease([temp.path().join("single.txt")])
        .await
        .expect("max-connections=1 disabled path-coordinated requests");
    assert_eq!(server.admission.path_wait_slots.available_permits(), 0);
    drop(lease);
    assert_eq!(server.admission.path_wait_slots.available_permits(), 1);
}

#[tokio::test]
async fn request_path_leases_fail_fast_and_retain_admission_until_drop() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let mut args = authenticated_args(temp.path().to_path_buf(), state_dir.path());
    args.max_connections = 3;
    let server = Arc::new(Server::init_with_lifecycle(args, ServerLifecycle::new()).unwrap());
    assert_eq!(server.admission.path_wait_slots.available_permits(), 2);

    let observations = Arc::new(Mutex::new(Vec::new()));
    let observed = observations.clone();
    *server
        .admission
        .path_wait_acquire_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move |available| {
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(available);
    }));

    let blocked_path = temp.path().join("blocked.txt");
    let held = server
        .content
        .path_coordinator
        .acquire([&blocked_path])
        .await;
    let waiter = {
        let server = server.clone();
        let blocked_path = blocked_path.clone();
        tokio::spawn(async move {
            server
                .acquire_request_path_lease([blocked_path])
                .await
                .expect("the first request path lease was rejected")
        })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !observations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the admitted path waiter never entered the coordinator");
    tokio::task::yield_now().await;
    assert!(
        !waiter.is_finished(),
        "a conflicting waiter bypassed its lease"
    );
    assert_eq!(
        server.admission.path_wait_slots.available_permits(),
        1,
        "a blocked path waiter released admission early"
    );

    let unrelated = server
        .acquire_request_path_lease([temp.path().join("unrelated.txt")])
        .await
        .expect("an unrelated path did not use the remaining admission slot");
    assert_eq!(server.admission.path_wait_slots.available_permits(), 0);
    let entered_before_rejection = observations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    assert!(
        server
            .acquire_request_path_lease([temp.path().join("rejected.txt")])
            .await
            .is_none(),
        "a request bypassed saturated path admission"
    );
    assert_eq!(
        observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        entered_before_rejection,
        "an admission-rejected request entered the path coordinator"
    );

    drop(unrelated);
    assert_eq!(
        server.admission.path_wait_slots.available_permits(),
        1,
        "dropping one lease did not release exactly one permit"
    );
    let replacement = server
        .acquire_request_path_lease([temp.path().join("replacement.txt")])
        .await
        .expect("an unrelated path could not enter after permit release");
    assert_eq!(server.admission.path_wait_slots.available_permits(), 0);
    drop(replacement);

    drop(held);
    let waited = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the admitted waiter did not acquire after conflict release")
        .unwrap();
    assert_eq!(
        server.admission.path_wait_slots.available_permits(),
        1,
        "a newly acquired request lease released its attached permit"
    );
    drop(waited);
    assert_eq!(server.admission.path_wait_slots.available_permits(), 2);
}

#[tokio::test]
async fn cancelled_request_path_resolution_retains_admission_until_the_probe_finishes() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let mut args = authenticated_args(temp.path().to_path_buf(), state_dir.path());
    args.max_connections = 2;
    let server = Arc::new(Server::init_with_lifecycle(args, ServerLifecycle::new()).unwrap());
    assert_eq!(server.admission.path_wait_slots.available_permits(), 1);

    let first_parent = temp.path().join("a-blocked");
    let later_parent = temp.path().join("z-later");
    std::fs::create_dir(&first_parent).unwrap();
    std::fs::create_dir(&later_parent).unwrap();
    let first = first_parent.join("first.txt");
    let later = later_parent.join("later.txt");

    let _ = server.content.rooted_fs.take_resolved_path_prefix_probes();
    let (entered_sender, entered_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
    server
        .content
        .rooted_fs
        .inject_before_resolved_path_prefix_probe_once(1, move || {
            let _ = entered_sender.send(());
            release_receiver
                .recv()
                .expect("path-resolution probe release sender dropped");
        });

    let request = {
        let server = server.clone();
        let first = first.clone();
        tokio::spawn(async move { server.acquire_request_path_lease([first, later]).await })
    };
    tokio::time::timeout(Duration::from_secs(1), entered_receiver)
        .await
        .expect("request path resolution did not enter its blocking probe")
        .unwrap();
    assert_eq!(server.admission.path_wait_slots.available_permits(), 0);

    request.abort();
    assert!(matches!(request.await, Err(error) if error.is_cancelled()));
    assert_eq!(
        server.admission.path_wait_slots.available_permits(),
        0,
        "cancelling the waiter released path admission before its probe exited"
    );
    assert!(
        server
            .acquire_request_path_lease([temp.path().join("rejected.txt")])
            .await
            .is_none(),
        "a new request bypassed the cancelled probe's retained admission"
    );

    release_sender.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.admission.path_wait_slots.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("path admission was not restored after the blocking probe exited");
    assert_eq!(
        server.content.rooted_fs.take_resolved_path_prefix_probes(),
        1,
        "a cancelled waiter continued resolving later paths"
    );

    let recovered = tokio::time::timeout(
        Duration::from_secs(1),
        server.acquire_request_path_lease([&first]),
    )
    .await
    .expect("the cancelled waiter registration remained in the coordinator")
    .expect("path admission was not restored after the probe exited");
    drop(recovered);
    assert_eq!(server.admission.path_wait_slots.available_permits(), 1);
}

#[tokio::test]
async fn background_path_acquire_bypasses_saturated_request_admission() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let mut args = authenticated_args(temp.path().to_path_buf(), state_dir.path());
    args.max_connections = 2;
    let server = Server::init_with_lifecycle(args, ServerLifecycle::new()).unwrap();

    let request_lease = server
        .acquire_request_path_lease([temp.path().join("request.txt")])
        .await
        .expect("request path admission was unexpectedly unavailable");
    assert_eq!(server.admission.path_wait_slots.available_permits(), 0);

    let background_lease = tokio::time::timeout(
        Duration::from_secs(1),
        server
            .content
            .path_coordinator
            .acquire([temp.path().join("background.txt")]),
    )
    .await
    .expect("background path acquisition was limited by request admission");
    assert_eq!(server.admission.path_wait_slots.available_permits(), 0);

    drop(background_lease);
    drop(request_lease);
    assert_eq!(server.admission.path_wait_slots.available_permits(), 1);
}

#[tokio::test]
async fn root_containment_guard_probes_a_deep_missing_path_once() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let server = Server::init_with_lifecycle(
        authenticated_args(temp.path().to_path_buf(), state_dir.path()),
        ServerLifecycle::new(),
    )
    .unwrap();

    let mut target = temp.path().to_path_buf();
    for index in 0..64 {
        target.push(format!("existing-{index}"));
        std::fs::create_dir(&target).unwrap();
    }
    for index in 0..64 {
        target.push(format!("missing-{index}"));
    }

    let probes = Arc::new(Mutex::new(Vec::new()));
    let observed = probes.clone();
    *server
        .admission
        .root_containment_probe_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move |path| {
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(path.to_path_buf());
    }));

    assert!(
        !server.guard_root_contained(&target).await.unwrap(),
        "an ordinary missing tail was classified as a root escape"
    );
    assert_eq!(
        *probes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [target],
        "root containment regressed to probing missing ancestors"
    );
}

#[tokio::test]
async fn root_containment_guard_preserves_miss_and_symlink_semantics() {
    let temp = assert_fs::TempDir::new().unwrap();
    let outside = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let server = Server::init_with_lifecycle(
        authenticated_args(temp.path().to_path_buf(), state_dir.path()),
        ServerLifecycle::new(),
    )
    .unwrap();

    std::fs::create_dir(temp.path().join("inside")).unwrap();
    symlink("inside", temp.path().join("inside-link")).unwrap();
    symlink("missing-target", temp.path().join("dangling-link")).unwrap();
    symlink("looping-link", temp.path().join("looping-link")).unwrap();
    symlink(outside.path(), temp.path().join("outside-link")).unwrap();
    std::fs::write(temp.path().join("file-parent"), b"not a directory").unwrap();

    for path in [
        temp.path().join("missing/child"),
        temp.path().join("inside-link/missing/child"),
        temp.path().join("dangling-link/child"),
        temp.path().join("file-parent/child"),
    ] {
        assert!(
            !server.guard_root_contained(&path).await.unwrap(),
            "contained miss was classified as a root escape: {}",
            path.display()
        );
    }

    assert!(
        server
            .guard_root_contained(&temp.path().join("outside-link/missing"))
            .await
            .unwrap(),
        "a root-escaping symlink was not hidden"
    );
    let loop_error = server
        .guard_root_contained(&temp.path().join("looping-link"))
        .await
        .expect_err("a symlink loop must remain an error for the containment guard");
    assert_eq!(
        loop_error.raw_os_error(),
        Some(rustix::io::Errno::LOOP.raw_os_error())
    );

    assert!(
        server
            .route_metadata(&temp.path().join("dangling-link"))
            .await
            .unwrap()
            .is_some(),
        "a final dangling symlink must remain manageable"
    );
    assert!(
        server
            .route_metadata(&temp.path().join("looping-link"))
            .await
            .unwrap()
            .is_some(),
        "a final looping symlink must remain manageable"
    );
    assert!(
        server
            .route_metadata(&temp.path().join("outside-link"))
            .await
            .unwrap()
            .is_none(),
        "a final root-escaping symlink must remain hidden"
    );
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

#[tokio::test]
async fn late_request_stops_waiting_when_shutdown_holds_the_gate() {
    let lifecycle = ServerLifecycle::new();
    let exclusive = lifecycle.request_gate.clone().write_owned().await;
    let waiter = {
        let lifecycle = lifecycle.clone();
        tokio::spawn(async move { lifecycle.enter_request().await.is_none() })
    };
    tokio::task::yield_now().await;

    lifecycle.shutdown.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("late request remained blocked behind the shutdown gate")
            .expect("late request task panicked"),
        "late request entered after shutdown cancellation"
    );
    drop(exclusive);
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

#[tokio::test]
async fn durable_state_path_scans_fail_fast_without_filling_the_actor_queue() {
    const SCAN_REQUESTS: usize = 300;

    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let server = Arc::new(
        Server::init_with_lifecycle(
            authenticated_args(temp.path().to_path_buf(), state_dir.path()),
            ServerLifecycle::new(),
        )
        .unwrap(),
    );
    let release_actor = server.state.state_store.block_actor_for_test().unwrap();
    let mut scans = Vec::with_capacity(SCAN_REQUESTS);
    for _ in 0..SCAN_REQUESTS {
        let server = server.clone();
        scans.push(tokio::spawn(async move {
            server
                .has_persisted_path_conflict(&[server.content.args.serve_path.as_path()])
                .await
        }));
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let rejected = scans.iter().filter(|scan| scan.is_finished()).count();
            if rejected == SCAN_REQUESTS - STATE_PATH_SCAN_CAPACITY {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("excess durable-state scans did not fail fast");
    assert_eq!(
        server.admission.state_path_scan_slots.available_permits(),
        0,
        "the admitted scans did not retain their permits while awaiting the actor"
    );

    let error = tokio::time::timeout(
        Duration::from_millis(100),
        server.has_persisted_path_descendant(temp.path()),
    )
    .await
    .expect("a saturated durable-state scan waited instead of failing fast")
    .expect_err("the shared durable-state scan limit was bypassed");
    assert_eq!(error.to_string(), STATE_PATH_SCAN_ADMISSION_ERROR);

    // `probe_readiness` sends its command on the first poll. Pending therefore
    // proves the actor queue still has room behind all admitted scans; a full
    // queue would return `StateStoreDispatchError::QueueFull` immediately.
    let state_store = server.state.state_store.clone();
    let mut readiness = Box::pin(state_store.probe_readiness());
    assert!(
        matches!(poll!(readiness.as_mut()), Poll::Pending),
        "durable-state scans filled the actor queue"
    );

    release_actor.send(()).unwrap();
    readiness.await.unwrap();
    let mut admitted = 0;
    let mut rejected = 0;
    for scan in scans {
        match scan.await.unwrap() {
            Ok(false) => admitted += 1,
            Ok(true) => panic!("an empty durable state store reported a path conflict"),
            Err(error) => {
                assert_eq!(error.to_string(), STATE_PATH_SCAN_ADMISSION_ERROR);
                rejected += 1;
            }
        }
    }
    assert_eq!(admitted, STATE_PATH_SCAN_CAPACITY);
    assert_eq!(rejected, SCAN_REQUESTS - STATE_PATH_SCAN_CAPACITY);
    assert_eq!(
        server.admission.state_path_scan_slots.available_permits(),
        STATE_PATH_SCAN_CAPACITY,
        "completed scans retained admission permits"
    );
}

#[tokio::test]
async fn cancelled_state_scans_retain_admission_until_actor_commands_finish() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let server = Arc::new(
        Server::init_with_lifecycle(
            authenticated_args(temp.path().to_path_buf(), state_dir.path()),
            ServerLifecycle::new(),
        )
        .unwrap(),
    );
    let release_actor = server.state.state_store.block_actor_for_test().unwrap();
    let mut scans = Vec::with_capacity(STATE_PATH_SCAN_CAPACITY);
    for index in 0..STATE_PATH_SCAN_CAPACITY {
        let server = server.clone();
        scans.push(tokio::spawn(async move {
            let path = server
                .content
                .args
                .serve_path
                .join(format!("actor-scan-{index}"));
            server.has_persisted_path_conflict(&[&path]).await
        }));
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        while server.admission.state_path_scan_slots.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("state scans did not submit their actor commands");
    for scan in &scans {
        scan.abort();
    }
    for scan in scans {
        assert!(scan.await.unwrap_err().is_cancelled());
    }

    assert_eq!(
        server.admission.state_path_scan_slots.available_permits(),
        0,
        "cancelled scan waiters released permits while actor commands were still queued"
    );
    let error = server
        .has_persisted_path_descendant(temp.path())
        .await
        .expect_err("a new scan bypassed actor-owned admission leases");
    assert_eq!(error.to_string(), STATE_PATH_SCAN_ADMISSION_ERROR);

    release_actor.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.admission.state_path_scan_slots.available_permits() != STATE_PATH_SCAN_CAPACITY
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor commands retained state scan permits after completing");
}

#[tokio::test]
async fn cancelled_state_scan_retains_admission_until_blocking_lookup_finishes() {
    let temp = assert_fs::TempDir::new().unwrap();
    let state_dir = private_state_dir();
    let server = Arc::new(
        Server::init_with_lifecycle(
            authenticated_args(temp.path().to_path_buf(), state_dir.path()),
            ServerLifecycle::new(),
        )
        .unwrap(),
    );

    let protected = temp.path().join("protected.txt");
    std::fs::write(&protected, "protected").unwrap();
    let PreparePurge::Prepared(_prepared) = server.prepare_purge("user", &protected).await.unwrap()
    else {
        panic!("durable purge store unexpectedly full");
    };
    let scan_parent = temp.path().join("scan-source");
    std::fs::create_dir(&scan_parent).unwrap();
    let scan_path = scan_parent.join("item.txt");

    let _ = server.content.rooted_fs.take_resolved_path_prefix_probes();
    let (entered_sender, entered_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
    server
        .content
        .rooted_fs
        .inject_before_resolved_path_prefix_probe_once(1, move || {
            let _ = entered_sender.send(());
            release_receiver
                .recv()
                .expect("blocking state-path lookup release sender dropped");
        });

    let mut held_permits = Vec::new();
    for _ in 1..STATE_PATH_SCAN_CAPACITY {
        held_permits.push(
            server
                .admission
                .state_path_scan_slots
                .clone()
                .acquire_owned()
                .await
                .unwrap(),
        );
    }
    let scan = {
        let server = server.clone();
        tokio::spawn(async move { server.has_persisted_path_conflict(&[&scan_path]).await })
    };
    tokio::time::timeout(Duration::from_secs(1), entered_receiver)
        .await
        .expect("state scan did not enter its blocking path lookup")
        .unwrap();
    assert_eq!(
        server.admission.state_path_scan_slots.available_permits(),
        0
    );

    scan.abort();
    assert!(scan.await.unwrap_err().is_cancelled());
    assert_eq!(
        server.admission.state_path_scan_slots.available_permits(),
        0,
        "cancelling a scan waiter released its in-flight blocking lookup permit"
    );
    let error = server
        .has_persisted_path_descendant(temp.path())
        .await
        .expect_err("a new scan bypassed a blocking lookup's admission lease");
    assert_eq!(error.to_string(), STATE_PATH_SCAN_ADMISSION_ERROR);

    release_sender.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.admission.state_path_scan_slots.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed blocking lookup retained its state scan permit");
    drop(held_permits);
    assert_eq!(
        server.admission.state_path_scan_slots.available_permits(),
        STATE_PATH_SCAN_CAPACITY
    );
}
