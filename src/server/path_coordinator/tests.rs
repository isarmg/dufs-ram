use super::*;

fn state_path(path: &str, allows_exact_replacement: bool) -> StateBlockingPath {
    StateBlockingPath {
        path: PathBuf::from(path),
        allows_exact_replacement,
    }
}
use std::{os::unix::fs::symlink, sync::Arc, time::Duration};

fn coordinator() -> (assert_fs::TempDir, PathCoordinator) {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    let coordinator = PathCoordinator::new(rooted_fs);
    (temp, coordinator)
}

async fn wait_for_resolutions(receiver: &mut watch::Receiver<u64>, target: u64) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if *receiver.borrow_and_update() >= target {
                return;
            }
            receiver.changed().await.unwrap();
        }
    })
    .await
    .expect("path resolution did not reach the expected attempt");
}

#[tokio::test]
async fn sibling_subtrees_can_run_concurrently() {
    let (temp, coordinator) = coordinator();
    let first = coordinator.acquire([temp.path().join("a/file")]).await;
    let second = tokio::time::timeout(
        Duration::from_millis(100),
        coordinator.acquire([temp.path().join("b/file")]),
    )
    .await;
    assert!(second.is_ok());
    drop(first);
}

#[tokio::test]
async fn resolving_waiter_does_not_block_an_unrelated_lexical_path() {
    let (temp, coordinator) = coordinator();
    let slow = normalize_key(&temp.path().join("slow"));
    let unrelated = normalize_key(&temp.path().join("unrelated"));
    std::fs::create_dir(&slow).unwrap();
    std::fs::create_dir(&unrelated).unwrap();

    let _slow_registration =
        WaiterRegistration::new(coordinator.inner.clone(), std::slice::from_ref(&slow));
    let unrelated_resolved = coordinator
        .inner
        .rooted_fs
        .resolved_path_key(&unrelated)
        .await
        .unwrap();
    let unrelated_request = [LeaseKey {
        lexical: unrelated.clone(),
        resolved: unrelated_resolved,
    }];
    let expected_epoch = coordinator.inner.lease_epoch.load(Ordering::Acquire);
    let mut unrelated_registration =
        WaiterRegistration::new(coordinator.inner.clone(), std::slice::from_ref(&unrelated));
    let unrelated_lease = match coordinator.try_acquire(
        &unrelated_request,
        expected_epoch,
        unrelated_registration.id,
    ) {
        AcquireAttempt::Acquired(lease) => lease,
        _ => panic!("a resolving waiter globally blocked an unrelated path"),
    };
    unrelated_registration.disarm();
    drop(unrelated_lease);

    let descendant = slow.join("child");
    let descendant_resolved = coordinator
        .inner
        .rooted_fs
        .resolved_path_key(&descendant)
        .await
        .unwrap();
    let descendant_request = [LeaseKey {
        lexical: descendant.clone(),
        resolved: descendant_resolved,
    }];
    let expected_epoch = coordinator.inner.lease_epoch.load(Ordering::Acquire);
    let descendant_registration =
        WaiterRegistration::new(coordinator.inner.clone(), std::slice::from_ref(&descendant));
    assert!(matches!(
        coordinator.try_acquire(
            &descendant_request,
            expected_epoch,
            descendant_registration.id,
        ),
        AcquireAttempt::Blocked
    ));
}

#[tokio::test]
async fn ancestor_waits_for_descendant() {
    let (temp, coordinator) = coordinator();
    let coordinator = Arc::new(coordinator);
    let descendant = coordinator.acquire([temp.path().join("a/file")]).await;
    let ancestor = temp.path().join("a");
    let waiter = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.acquire([ancestor]).await })
    };

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());
    drop(descendant);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn earlier_conflicting_waiter_cannot_be_starved_by_later_siblings() {
    let (temp, coordinator) = coordinator();
    std::fs::create_dir(temp.path().join("a")).unwrap();
    let coordinator = Arc::new(coordinator);
    let held = coordinator.acquire([temp.path().join("a/x")]).await;
    let mut resolutions = coordinator.inner.resolutions.subscribe();
    let initial_resolutions = *resolutions.borrow_and_update();

    let ancestor_waiter = {
        let coordinator = coordinator.clone();
        let ancestor = temp.path().join("a");
        tokio::spawn(async move { coordinator.acquire([ancestor]).await })
    };
    wait_for_resolutions(&mut resolutions, initial_resolutions + 1).await;
    assert!(!ancestor_waiter.is_finished());

    let mut sibling_waiter = {
        let coordinator = coordinator.clone();
        let sibling = temp.path().join("a/y");
        tokio::spawn(async move { coordinator.acquire([sibling]).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !sibling_waiter.is_finished(),
        "a later sibling bypassed the earlier ancestor waiter"
    );

    drop(held);
    let ancestor = tokio::time::timeout(Duration::from_secs(1), ancestor_waiter)
        .await
        .expect("the earlier ancestor waiter did not acquire first")
        .unwrap();
    assert!(
        !sibling_waiter.is_finished(),
        "the later sibling overlapped the acquired ancestor lease"
    );
    drop(ancestor);
    tokio::time::timeout(Duration::from_secs(1), &mut sibling_waiter)
        .await
        .expect("the later sibling did not acquire after the ancestor released")
        .unwrap();
}

#[tokio::test]
async fn multi_path_lease_is_acquired_without_deadlock() {
    let (temp, coordinator) = coordinator();
    let coordinator = Arc::new(coordinator);
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    let first = coordinator.acquire([&a, &b]).await;
    let waiter = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.acquire([b, a]).await })
    };

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());
    drop(first);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn symlink_aliases_for_the_same_entry_are_serialized() {
    let (temp, coordinator) = coordinator();
    std::fs::create_dir(temp.path().join("target")).unwrap();
    symlink("target", temp.path().join("alias")).unwrap();
    let coordinator = Arc::new(coordinator);
    let first = coordinator
        .acquire([temp.path().join("target/file.txt")])
        .await;
    let alias = temp.path().join("alias/file.txt");
    let waiter = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.acquire([alias]).await })
    };

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());
    drop(first);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn real_directory_mutation_conflicts_with_alias_descendant() {
    let (temp, coordinator) = coordinator();
    std::fs::create_dir(temp.path().join("target")).unwrap();
    symlink("target", temp.path().join("alias")).unwrap();
    let coordinator = Arc::new(coordinator);
    let directory = coordinator.acquire([temp.path().join("target")]).await;
    let alias_child = temp.path().join("alias/file.txt");
    let waiter = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.acquire([alias_child]).await })
    };

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());
    drop(directory);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn persisted_state_paths_detect_lexical_and_symlink_move_conflicts() {
    let (temp, coordinator) = coordinator();
    std::fs::create_dir(temp.path().join("target")).unwrap();
    std::fs::create_dir(temp.path().join("unrelated")).unwrap();
    symlink("target", temp.path().join("alias")).unwrap();

    assert!(
        coordinator
            .conflicts_with_state_paths(
                &temp.path().join("target"),
                &[state_path("target/lexical.txt", false)],
            )
            .await
    );
    assert!(
        coordinator
            .conflicts_with_state_paths(
                &temp.path().join("target"),
                &[state_path("alias/semantic.txt", false)],
            )
            .await
    );
    assert!(
        !coordinator
            .conflicts_with_state_paths(
                &temp.path().join("target"),
                &[state_path("unrelated/file.txt", false)],
            )
            .await
    );

    let exact = temp.path().join("target/exact.txt");
    assert!(
        !coordinator
            .has_state_path_descendant(&exact, &[state_path("target/exact.txt", true)])
            .await,
        "a Running upload target equal to a fresh PUT target is replaceable"
    );
    assert!(
        coordinator
            .has_state_path_descendant(&exact, &[state_path("target/exact.txt", false)])
            .await,
        "an exact CommitStarted or purge obligation remains protected"
    );
    assert!(
        coordinator
            .has_state_path_descendant(
                &temp.path().join("target"),
                &[state_path("alias/semantic.txt", false)],
            )
            .await,
        "a persisted path reached through a symlink remains a descendant"
    );
}

#[tokio::test]
async fn persisted_state_path_resolution_failure_blocks_move() {
    let (temp, coordinator) = coordinator();
    std::fs::create_dir(temp.path().join("source")).unwrap();
    coordinator
        .inner
        .resolution_failures
        .store(1, Ordering::SeqCst);

    assert!(
        coordinator
            .conflicts_with_state_paths(
                &temp.path().join("source"),
                &[state_path("unrelated/file.txt", false)],
            )
            .await
    );

    symlink("/", temp.path().join("escape")).unwrap();
    assert!(
        coordinator
            .has_state_path_descendant(
                &temp.path().join("source"),
                &[state_path("escape/etc/passwd", true)],
            )
            .await,
        "directional PUT admission must fail closed when a state path cannot be resolved"
    );
}

#[tokio::test]
async fn alias_retargeted_while_waiting_is_resolved_again_before_acquire() {
    let (temp, coordinator) = coordinator();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    symlink("a", &alias).unwrap();

    let coordinator = Arc::new(coordinator);
    let b_lease = coordinator.acquire([b.join("file.txt")]).await;
    let retarget_lease = coordinator
        .acquire([a.join("file.txt"), alias.clone()])
        .await;
    let mut resolutions = coordinator.inner.resolutions.subscribe();
    let initial_resolutions = *resolutions.borrow_and_update();
    let alias_file = alias.join("file.txt");
    let mut waiter = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.acquire([alias_file]).await })
    };

    wait_for_resolutions(&mut resolutions, initial_resolutions + 1).await;
    assert!(!waiter.is_finished());

    std::fs::remove_file(&alias).unwrap();
    symlink("b", &alias).unwrap();
    drop(retarget_lease);

    wait_for_resolutions(&mut resolutions, initial_resolutions + 2).await;
    assert!(
        !waiter.is_finished(),
        "the alias waiter acquired concurrently with the retargeted real path"
    );

    drop(b_lease);
    tokio::time::timeout(Duration::from_secs(1), &mut waiter)
        .await
        .expect("alias waiter remained blocked after the real path lease was released")
        .unwrap();
}

#[tokio::test]
async fn unrelated_lease_start_does_not_invalidate_resolved_keys() {
    let (temp, coordinator) = coordinator();
    let target = temp.path().join("target");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&target).unwrap();
    symlink("target", &alias).unwrap();

    let lexical = normalize_key(&alias.join("file.txt"));
    let expected_epoch = coordinator.inner.lease_epoch.load(Ordering::Acquire);
    let resolved = coordinator
        .inner
        .rooted_fs
        .resolved_path_key(&lexical)
        .await
        .unwrap();
    let requested = [LeaseKey { lexical, resolved }];

    let intervening_lease = coordinator.acquire([temp.path().join("unrelated")]).await;
    let mut registration = WaiterRegistration::new(
        coordinator.inner.clone(),
        std::slice::from_ref(&requested[0].lexical),
    );
    let requested_lease = match coordinator.try_acquire(&requested, expected_epoch, registration.id)
    {
        AcquireAttempt::Acquired(lease) => lease,
        _ => panic!("an unrelated lease start invalidated a resolved semantic key"),
    };
    registration.disarm();
    assert_eq!(
        coordinator
            .inner
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2,
        "unrelated semantic keys should be leased concurrently"
    );
    drop(requested_lease);
    drop(intervening_lease);
}

#[tokio::test]
async fn semantic_resolution_failure_never_grants_a_lexical_only_alias_lease() {
    let (temp, coordinator) = coordinator();
    std::fs::create_dir(temp.path().join("target")).unwrap();
    symlink("target", temp.path().join("alias")).unwrap();
    let coordinator = Arc::new(coordinator);
    let held = coordinator
        .acquire([temp.path().join("target/file.txt")])
        .await;
    coordinator
        .inner
        .resolution_failures
        .store(1, Ordering::SeqCst);
    let attempts_before = coordinator.inner.resolution_attempts.load(Ordering::SeqCst);
    let alias = temp.path().join("alias/file.txt");
    let mut waiter = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move { coordinator.acquire([alias]).await })
    };

    tokio::time::timeout(Duration::from_secs(1), async {
        while coordinator.inner.resolution_attempts.load(Ordering::SeqCst) == attempts_before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the injected resolution failure was not exercised");
    tokio::task::yield_now().await;
    assert!(
        !waiter.is_finished(),
        "a failed semantic resolution must not grant an alias lease"
    );

    drop(held);
    tokio::time::timeout(Duration::from_secs(1), &mut waiter)
        .await
        .expect("alias waiter did not acquire after the conflicting lease was released")
        .unwrap();
}

#[tokio::test]
async fn persistent_semantic_resolution_failure_uses_a_finite_global_lease() {
    let (temp, coordinator) = coordinator();
    coordinator
        .inner
        .resolution_failures
        .store(usize::MAX, Ordering::SeqCst);

    let lease = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        coordinator.acquire([temp.path().join("permanently-unresolved")]),
    )
    .await
    .expect("persistent resolution errors must not leave a mutation task running forever");

    assert_eq!(
        coordinator
            .inner
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    drop(lease);
}
