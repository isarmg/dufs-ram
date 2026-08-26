use super::{
    RequestPathAdmissionLease, StatePathScanLease,
    rooted_fs::{ResolvedPathKey, RootedFs},
    state_store::StateBlockingPath,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::watch;

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

#[derive(Clone)]
pub(super) struct PathCoordinator {
    inner: Arc<CoordinatorInner>,
}

struct CoordinatorInner {
    rooted_fs: RootedFs,
    leases: Mutex<BTreeMap<u64, Vec<LeaseKey>>>,
    waiters: Mutex<BTreeMap<u64, WaiterState>>,
    next_id: AtomicU64,
    next_waiter_id: AtomicU64,
    lease_epoch: AtomicU64,
    changes: watch::Sender<u64>,
    #[cfg(test)]
    resolutions: watch::Sender<u64>,
    #[cfg(test)]
    resolution_failures: AtomicUsize,
    #[cfg(test)]
    resolution_attempts: AtomicUsize,
}

#[derive(Clone, Debug)]
struct LeaseKey {
    lexical: PathBuf,
    resolved: ResolvedPathKey,
}

#[derive(Clone, Copy)]
enum StatePathScanRelation {
    SymmetricConflict,
    StrictDescendant,
}

pub(super) struct StatePathScanner {
    inner: Arc<CoordinatorInner>,
    sources: Vec<PathBuf>,
    resolved_sources: Option<Vec<LeaseKey>>,
    relation: StatePathScanRelation,
    scan_lease: StatePathScanLease,
}

enum WaiterState {
    Resolving(Vec<PathBuf>),
    Resolved(Vec<LeaseKey>),
}

pub(super) struct PathLease {
    inner: Arc<CoordinatorInner>,
    id: u64,
    _request_admission: Option<RequestPathAdmissionLease>,
}

struct WaiterRegistration {
    inner: Arc<CoordinatorInner>,
    id: u64,
    active: bool,
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
                waiters: Mutex::new(BTreeMap::new()),
                next_id: AtomicU64::new(1),
                next_waiter_id: AtomicU64::new(1),
                lease_epoch: AtomicU64::new(0),
                changes,
                #[cfg(test)]
                resolutions,
                #[cfg(test)]
                resolution_failures: AtomicUsize::new(0),
                #[cfg(test)]
                resolution_attempts: AtomicUsize::new(0),
            }),
        }
    }

    pub(super) async fn acquire<I, P>(&self, paths: I) -> PathLease
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.acquire_inner(paths, None).await
    }

    pub(super) async fn acquire_for_request<I, P>(
        &self,
        paths: I,
        admission: RequestPathAdmissionLease,
    ) -> PathLease
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.acquire_inner(paths, Some(admission)).await
    }

    async fn acquire_inner<I, P>(
        &self,
        paths: I,
        mut request_admission: Option<RequestPathAdmissionLease>,
    ) -> PathLease
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

        let mut registration =
            WaiterRegistration::new(self.inner.clone(), lexical_paths.as_slice());
        let mut changes = self.inner.changes.subscribe();
        let mut resolved_paths: Option<(u64, Vec<LeaseKey>)> = None;
        loop {
            changes.borrow_and_update();
            let expected_epoch = self.inner.lease_epoch.load(Ordering::Acquire);
            if resolved_paths
                .as_ref()
                .is_none_or(|(epoch, _)| *epoch != expected_epoch)
            {
                let previous_paths = resolved_paths.as_ref().map(|(_, paths)| paths.as_slice());
                if !registration
                    .mark_resolving_if_eligible(lexical_paths.as_slice(), previous_paths)
                {
                    if changes.changed().await.is_err() {
                        unreachable!("the path coordinator owns the change sender");
                    }
                    continue;
                }
                let mut paths = Vec::with_capacity(lexical_paths.len());
                for lexical in &lexical_paths {
                    let resolved = match self
                        .resolve_path_key(lexical, request_admission.clone())
                        .await
                    {
                        Ok(resolved) => resolved,
                        // Never drop down to lexical-only coordination. A
                        // global root key serializes this request with every
                        // semantic mutation, while still letting the actual
                        // rooted operation promptly return XDEV/ELOOP/EIO
                        // instead of waiting forever for a permanent error.
                        Err(_) => self.inner.rooted_fs.conservative_path_key(),
                    };
                    paths.push(LeaseKey {
                        lexical: lexical.clone(),
                        resolved,
                    });
                }
                #[cfg(test)]
                self.inner.resolutions.send_modify(|version| {
                    *version = version.wrapping_add(1);
                });
                resolved_paths = Some((expected_epoch, paths));
            }
            let paths = &resolved_paths
                .as_ref()
                .expect("the current lease epoch has resolved path keys")
                .1;

            match self.try_acquire(
                paths,
                expected_epoch,
                registration.id,
                &mut request_admission,
            ) {
                AcquireAttempt::Acquired(lease) => {
                    registration.disarm();
                    return lease;
                }
                AcquireAttempt::Stale => {
                    // Keep the old semantic key only as a conservative queue
                    // hint. The epoch check above always forces a fresh
                    // resolution before this waiter can acquire a lease.
                    continue;
                }
                AcquireAttempt::Blocked => {}
            }
            if changes.changed().await.is_err() {
                unreachable!("the path coordinator owns the change sender");
            }
        }
    }

    /// Check whether a persisted control-plane path conflicts with a source
    /// that is about to be moved. The caller must already own the source path
    /// lease, so no cooperating mutation can retarget a symlink between these
    /// resolutions and the eventual commit. Resolution failures deliberately
    /// use the global root key and therefore fail closed.
    #[cfg(test)]
    pub(super) async fn conflicts_with_state_paths(
        &self,
        source: &Path,
        state_paths: &[StateBlockingPath],
    ) -> bool {
        let mut scanner =
            self.state_path_conflict_scanner(&[source], StatePathScanLease::for_test());
        scanner.page_conflicts(state_paths.to_vec()).await
    }

    pub(super) fn state_path_conflict_scanner(
        &self,
        sources: &[&Path],
        scan_lease: StatePathScanLease,
    ) -> StatePathScanner {
        debug_assert!(!sources.is_empty());
        StatePathScanner {
            inner: self.inner.clone(),
            sources: sources.iter().map(|path| normalize_key(path)).collect(),
            resolved_sources: None,
            relation: StatePathScanRelation::SymmetricConflict,
            scan_lease,
        }
    }

    /// Check whether replacing `ancestor` would change the physical meaning
    /// of a durable control-plane path below it. Unlike the symmetric MOVE and
    /// DELETE admission check, equality follows the role attached to each
    /// state path: a fresh PUT may supersede an idle Running upload target,
    /// while CommitStarted uploads and purge intents remain protected.
    /// Resolution failures fail closed.
    #[cfg(test)]
    pub(super) async fn has_state_path_descendant(
        &self,
        ancestor: &Path,
        state_paths: &[StateBlockingPath],
    ) -> bool {
        let mut scanner =
            self.state_path_descendant_scanner(ancestor, StatePathScanLease::for_test());
        scanner.page_conflicts(state_paths.to_vec()).await
    }

    pub(super) fn state_path_descendant_scanner(
        &self,
        ancestor: &Path,
        scan_lease: StatePathScanLease,
    ) -> StatePathScanner {
        StatePathScanner {
            inner: self.inner.clone(),
            sources: vec![normalize_key(ancestor)],
            resolved_sources: None,
            relation: StatePathScanRelation::StrictDescendant,
            scan_lease,
        }
    }

    async fn resolve_path_key(
        &self,
        lexical: &Path,
        request_admission: Option<RequestPathAdmissionLease>,
    ) -> std::io::Result<ResolvedPathKey> {
        self.inner.begin_path_key_resolution()?;
        match request_admission {
            Some(admission) => {
                self.inner
                    .rooted_fs
                    .resolved_path_key_guarded(lexical, admission)
                    .await
            }
            None => self.inner.rooted_fs.resolved_path_key(lexical).await,
        }
    }

    fn try_acquire(
        &self,
        requested: &[LeaseKey],
        expected_epoch: u64,
        waiter_id: u64,
        request_admission: &mut Option<RequestPathAdmissionLease>,
    ) -> AcquireAttempt {
        let mut leases = self
            .inner
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.lease_epoch.load(Ordering::Acquire) != expected_epoch {
            return AcquireAttempt::Stale;
        }
        let mut waiters = self
            .inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(waiter) = waiters.get_mut(&waiter_id) else {
            return AcquireAttempt::Stale;
        };
        let became_resolved = matches!(waiter, WaiterState::Resolving(_));
        *waiter = WaiterState::Resolved(requested.to_vec());
        let blocked = leases.values().any(|held| paths_conflict(held, requested))
            || waiters
                .range(..waiter_id)
                .any(|(_, earlier)| waiter_conflicts(earlier, requested));
        if blocked {
            if became_resolved {
                self.inner.changes.send_modify(|version| {
                    *version = version.wrapping_add(1);
                });
            }
            return AcquireAttempt::Blocked;
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        waiters.remove(&waiter_id);
        leases.insert(id, requested.to_vec());
        // Starting a lease cannot make an already-resolved semantic key stale:
        // try_acquire holds the lease map lock while checking conflicts and
        // inserting this key. A mutation that can change resolution will bump
        // lease_epoch when its lease is released. Avoiding an epoch bump here
        // keeps unrelated waiters from repeating rooted filesystem lookups.
        // Still wake the queue because removing its earliest waiter can make a
        // later, unrelated request eligible to resolve or retry immediately.
        self.inner.changes.send_modify(|version| {
            *version = version.wrapping_add(1);
        });
        AcquireAttempt::Acquired(PathLease {
            inner: self.inner.clone(),
            id,
            _request_admission: request_admission.take(),
        })
    }
}

impl CoordinatorInner {
    #[inline]
    fn begin_path_key_resolution(&self) -> std::io::Result<()> {
        #[cfg(test)]
        {
            self.resolution_attempts.fetch_add(1, Ordering::SeqCst);
            if self
                .resolution_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(std::io::Error::other(
                    "injected semantic path resolution failure",
                ));
            }
        }
        Ok(())
    }
}

impl StatePathScanner {
    /// Compare one durable-state page. Empty pages deliberately do no semantic
    /// path resolution; the first non-empty page resolves and caches every
    /// source in the same blocking batch as its candidates. Later pages only
    /// resolve their candidates.
    pub(super) async fn page_conflicts(&mut self, state_paths: Vec<StateBlockingPath>) -> bool {
        if state_paths.is_empty() {
            return false;
        }

        let mut candidates = Vec::with_capacity(state_paths.len());
        for state_path in state_paths {
            let lexical = match self.inner.rooted_fs.resolve_state_path(&state_path.path) {
                Ok(path) => normalize_key(&path),
                Err(_) => return true,
            };
            candidates.push((lexical, state_path.allows_exact_replacement));
        }

        let resolving_sources = self.resolved_sources.is_none();
        let source_count = if resolving_sources {
            self.sources.len()
        } else {
            0
        };
        let mut batch_paths = Vec::with_capacity(source_count + candidates.len());
        if resolving_sources {
            batch_paths.extend(self.sources.iter().cloned());
        }
        batch_paths.extend(candidates.iter().map(|(path, _)| path.clone()));
        #[cfg(test)]
        for _ in &batch_paths {
            if self.inner.begin_path_key_resolution().is_err() {
                return true;
            }
        }

        let rooted_fs = self.inner.rooted_fs.clone();
        let resolved = match rooted_fs
            .resolved_path_keys_for_state_scan(batch_paths, self.scan_lease.clone())
            .await
        {
            Ok(resolved) => resolved,
            // On a non-empty page, mapping a failed source or candidate to the
            // conservative root key necessarily conflicts with the other side.
            // Returning immediately is therefore equivalent to the former
            // per-path fail-closed behavior and avoids needless later I/O.
            Err(_) => return true,
        };
        let mut resolved = resolved.into_iter();

        if resolving_sources {
            let source_keys = self
                .sources
                .iter()
                .cloned()
                .map(|lexical| LeaseKey {
                    lexical,
                    resolved: resolved
                        .next()
                        .expect("a state scan batch preserves its input length"),
                })
                .collect();
            self.resolved_sources = Some(source_keys);
        }

        let candidate_keys: Vec<_> = candidates
            .into_iter()
            .map(|(lexical, allows_exact_replacement)| {
                (
                    LeaseKey {
                        lexical,
                        resolved: resolved
                            .next()
                            .expect("a state scan batch preserves its input length"),
                    },
                    allows_exact_replacement,
                )
            })
            .collect();
        debug_assert!(resolved.next().is_none());
        let sources = self
            .resolved_sources
            .as_ref()
            .expect("a non-empty state scan page resolves its sources");

        match self.relation {
            StatePathScanRelation::SymmetricConflict => sources.iter().any(|source| {
                candidate_keys
                    .iter()
                    .any(|(candidate, _)| path_keys_conflict(source, candidate))
            }),
            StatePathScanRelation::StrictDescendant => {
                let ancestor = sources
                    .first()
                    .expect("a directional state scan has one source");
                candidate_keys
                    .iter()
                    .any(|(candidate, allows_exact_replacement)| {
                        if paths_identify_same_entry(ancestor, candidate) {
                            return !*allows_exact_replacement;
                        }
                        path_strictly_contains(ancestor, candidate)
                    })
            }
        }
    }
}

impl WaiterRegistration {
    fn new(inner: Arc<CoordinatorInner>, lexical_paths: &[PathBuf]) -> Self {
        let id = inner.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, WaiterState::Resolving(lexical_paths.to_vec()));
        Self {
            inner,
            id,
            active: true,
        }
    }

    fn mark_resolving_if_eligible(
        &self,
        lexical_paths: &[PathBuf],
        previous_paths: Option<&[LeaseKey]>,
    ) -> bool {
        let mut waiters = self
            .inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let blocked_by_earlier = waiters.range(..self.id).any(|(_, earlier)| {
            previous_paths.map_or_else(
                || waiter_lexically_conflicts(earlier, lexical_paths),
                |paths| waiter_conflicts(earlier, paths),
            )
        });
        if blocked_by_earlier {
            return false;
        }
        if let Some(waiter) = waiters.get_mut(&self.id) {
            *waiter = WaiterState::Resolving(lexical_paths.to_vec());
        }
        true
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for WaiterRegistration {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if self
            .inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id)
            .is_some()
        {
            self.inner.changes.send_modify(|version| {
                *version = version.wrapping_add(1);
            });
        }
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
            self.inner.lease_epoch.fetch_add(1, Ordering::Release);
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
        right
            .iter()
            .any(|right_path| path_keys_conflict(left_path, right_path))
    })
}

fn path_keys_conflict(left: &LeaseKey, right: &LeaseKey) -> bool {
    left.lexical.starts_with(&right.lexical)
        || right.lexical.starts_with(&left.lexical)
        || resolved_paths_conflict(&left.resolved, &right.resolved)
}

fn path_strictly_contains(ancestor: &LeaseKey, descendant: &LeaseKey) -> bool {
    (ancestor.lexical != descendant.lexical && descendant.lexical.starts_with(&ancestor.lexical))
        || resolved_path_strictly_contains(&ancestor.resolved, &descendant.resolved)
}

fn paths_identify_same_entry(left: &LeaseKey, right: &LeaseKey) -> bool {
    left.lexical == right.lexical
        || (left.resolved.resolved_parent == right.resolved.resolved_parent
            && left.resolved.unresolved_tail == right.resolved.unresolved_tail)
}

fn waiter_conflicts(waiter: &WaiterState, requested: &[LeaseKey]) -> bool {
    match waiter {
        // A resolving waiter does not own a mutation lease yet, so unrelated
        // lexical paths may safely overtake it. If one is actually a semantic
        // alias, the earlier resolver will either observe the active lease or
        // observe the epoch bump emitted when that mutation completes.
        WaiterState::Resolving(lexical_paths) => lexical_paths.iter().any(|left| {
            requested
                .iter()
                .any(|right| left.starts_with(&right.lexical) || right.lexical.starts_with(left))
        }),
        WaiterState::Resolved(paths) => paths_conflict(paths, requested),
    }
}

fn waiter_lexically_conflicts(waiter: &WaiterState, requested: &[PathBuf]) -> bool {
    match waiter {
        WaiterState::Resolving(lexical_paths) => lexical_paths.iter().any(|left| {
            requested
                .iter()
                .any(|right| left.starts_with(right) || right.starts_with(left))
        }),
        WaiterState::Resolved(paths) => paths.iter().any(|left| {
            requested
                .iter()
                .any(|right| left.lexical.starts_with(right) || right.starts_with(&left.lexical))
        }),
    }
}

fn resolved_paths_conflict(left: &ResolvedPathKey, right: &ResolvedPathKey) -> bool {
    anchored_tail_is_prefix(left, right)
        || anchored_tail_is_prefix(right, left)
        || left
            .target_directory
            .is_some_and(|identity| right.ancestor_directories.contains(&identity))
        || right
            .target_directory
            .is_some_and(|identity| left.ancestor_directories.contains(&identity))
}

fn resolved_path_strictly_contains(
    ancestor: &ResolvedPathKey,
    descendant: &ResolvedPathKey,
) -> bool {
    (ancestor.unresolved_tail.len() < descendant.unresolved_tail.len()
        && anchored_tail_is_prefix(ancestor, descendant))
        || ancestor
            .target_directory
            .is_some_and(|identity| descendant.ancestor_directories.contains(&identity))
}

fn anchored_tail_is_prefix(left: &ResolvedPathKey, right: &ResolvedPathKey) -> bool {
    left.resolved_parent == right.resolved_parent
        && left.unresolved_tail.len() <= right.unresolved_tail.len()
        && left
            .unresolved_tail
            .iter()
            .zip(&right.unresolved_tail)
            .all(|(left, right)| left == right)
}

#[cfg(test)]
mod tests;
