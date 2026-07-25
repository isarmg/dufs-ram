use super::rooted_fs::{FileIdentity, ResolvedPathKey, RootedFs, SemanticPathComponent};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::watch;

#[derive(Clone)]
pub(super) struct PathCoordinator {
    inner: Arc<CoordinatorInner>,
}

struct CoordinatorInner {
    rooted_fs: RootedFs,
    leases: Mutex<BTreeMap<u64, Vec<LeaseKey>>>,
    next_id: AtomicU64,
    changes: watch::Sender<u64>,
    #[cfg(test)]
    resolutions: watch::Sender<u64>,
}

#[derive(Clone, Debug)]
struct LeaseKey {
    lexical: PathBuf,
    resolved: Option<ResolvedPathKey>,
}

pub(super) struct PathLease {
    inner: Arc<CoordinatorInner>,
    id: u64,
}

enum AcquireAttempt {
    Acquired(PathLease),
    Blocked,
    Stale,
}

impl PathCoordinator {
    pub(super) fn new(rooted_fs: RootedFs) -> Self {
        let (changes, _) = watch::channel(0);
        #[cfg(test)]
        let (resolutions, _) = watch::channel(0);
        Self {
            inner: Arc::new(CoordinatorInner {
                rooted_fs,
                leases: Mutex::new(BTreeMap::new()),
                next_id: AtomicU64::new(1),
                changes,
                #[cfg(test)]
                resolutions,
            }),
        }
    }

    pub(super) async fn acquire<I, P>(&self, paths: I) -> PathLease
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut lexical_paths: Vec<PathBuf> = paths
            .into_iter()
            .map(|path| normalize_key(path.as_ref()))
            .collect();
        lexical_paths.sort();
        lexical_paths.dedup();
        debug_assert!(!lexical_paths.is_empty());

        let mut changes = self.inner.changes.subscribe();
        loop {
            let expected_version = *changes.borrow_and_update();
            let mut paths = Vec::with_capacity(lexical_paths.len());
            for lexical in &lexical_paths {
                let resolved = self.inner.rooted_fs.resolved_path_key(lexical).await.ok();
                paths.push(LeaseKey {
                    lexical: lexical.clone(),
                    resolved,
                });
            }
            #[cfg(test)]
            self.inner.resolutions.send_modify(|version| {
                *version = version.wrapping_add(1);
            });

            match self.try_acquire(&paths, expected_version) {
                AcquireAttempt::Acquired(lease) => return lease,
                AcquireAttempt::Stale => continue,
                AcquireAttempt::Blocked => {}
            }
            if changes.changed().await.is_err() {
                unreachable!("the path coordinator owns the change sender");
            }
        }
    }

    fn try_acquire(&self, requested: &[LeaseKey], expected_version: u64) -> AcquireAttempt {
        let mut leases = self
            .inner
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_version = *self.inner.changes.borrow();
        if current_version != expected_version {
            return AcquireAttempt::Stale;
        }
        if leases.values().any(|held| paths_conflict(held, requested)) {
            return AcquireAttempt::Blocked;
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        leases.insert(id, requested.to_vec());
        self.inner.changes.send_modify(|version| {
            *version = version.wrapping_add(1);
        });
        AcquireAttempt::Acquired(PathLease {
            inner: self.inner.clone(),
            id,
        })
    }
}

impl Drop for PathLease {
    fn drop(&mut self) {
        let mut leases = self
            .inner
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if leases.remove(&self.id).is_some() {
            self.inner.changes.send_modify(|version| {
                *version = version.wrapping_add(1);
            });
        }
    }
}

fn normalize_key(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        normalized.push(component);
    }
    normalized
}

fn paths_conflict(left: &[LeaseKey], right: &[LeaseKey]) -> bool {
    left.iter().any(|left_path| {
        right.iter().any(|right_path| {
            left_path.lexical.starts_with(&right_path.lexical)
                || right_path.lexical.starts_with(&left_path.lexical)
                || match (&left_path.resolved, &right_path.resolved) {
                    (Some(left), Some(right)) => resolved_paths_conflict(left, right),
                    _ => false,
                }
        })
    })
}

fn resolved_paths_conflict(left: &ResolvedPathKey, right: &ResolvedPathKey) -> bool {
    components_are_prefix(&left.components, &right.components)
        || components_are_prefix(&right.components, &left.components)
        || left
            .target_directory
            .is_some_and(|identity| contains_directory(&right.components, identity))
        || right
            .target_directory
            .is_some_and(|identity| contains_directory(&left.components, identity))
}

fn components_are_prefix(left: &[SemanticPathComponent], right: &[SemanticPathComponent]) -> bool {
    left.len() <= right.len() && left.iter().zip(right).all(|(left, right)| left == right)
}

fn contains_directory(components: &[SemanticPathComponent], identity: FileIdentity) -> bool {
    components.iter().any(
        |component| matches!(component, SemanticPathComponent::Directory(value) if *value == identity),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn lease_version_change_between_resolution_and_insert_rejects_stale_keys() {
        let (temp, coordinator) = coordinator();
        let target = temp.path().join("target");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&target).unwrap();
        symlink("target", &alias).unwrap();

        let lexical = normalize_key(&alias.join("file.txt"));
        let expected_version = *coordinator.inner.changes.borrow();
        let resolved = coordinator
            .inner
            .rooted_fs
            .resolved_path_key(&lexical)
            .await
            .unwrap();
        let requested = [LeaseKey {
            lexical,
            resolved: Some(resolved),
        }];

        let intervening_lease = coordinator.acquire([temp.path().join("unrelated")]).await;
        assert!(matches!(
            coordinator.try_acquire(&requested, expected_version),
            AcquireAttempt::Stale
        ));
        assert_eq!(
            coordinator
                .inner
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "stale semantic keys must not be inserted"
        );
        drop(intervening_lease);
    }
}
