use super::super::{
    blocking_io::blocking_io_gate,
    internal_names::is_internal_name,
    rooted_fs::{DirectoryCursor, DirectoryVisitProgress, RootedDirEntry, RootedFs},
};
use super::{
    BoundedReserveError, DirectorySnapshot, ListingError, ListingProblem, ListingResult,
    collection_allocation_bytes, safe_relative_path, try_reserve_bounded_vec_slot,
};

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
    time::{Duration, Instant},
};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub(in crate::server) struct DirectoryWalk {
    pub(in crate::server) work_tasks: TaskTracker,
    pub(in crate::server) running: Arc<AtomicBool>,
    pub(in crate::server) cancellation: CancellationToken,
    pub(in crate::server) path: PathBuf,
    pub(in crate::server) serve_path: PathBuf,
    pub(in crate::server) rooted_fs: RootedFs,
    pub(in crate::server) max_entries: usize,
    pub(in crate::server) max_depth: usize,
    pub(in crate::server) max_working_bytes: usize,
    pub(in crate::server) max_duration: Duration,
    pub(in crate::server) permit: Option<Arc<OwnedSemaphorePermit>>,
}

pub(in crate::server) fn spawn_directory_blocking<F, T>(
    work_tasks: &TaskTracker,
    permit: Option<Arc<OwnedSemaphorePermit>>,
    task: F,
) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let gate = blocking_io_gate().clone();
    work_tasks.spawn(async move {
        gate.run(move || {
            let _permit = permit;
            task()
        })
        .await
        .unwrap_or_else(|error| panic!("blocking directory worker failed: {error}"))
    })
}

#[derive(Clone, Copy)]
pub(in crate::server) struct CollectionByteBudget {
    pub(in crate::server) max_bytes: usize,
    pub(in crate::server) operation: &'static str,
    pub(in crate::server) reason: &'static str,
    pub(in crate::server) problem: ListingProblem,
    pub(in crate::server) allocation_problem: ListingProblem,
}

struct DirectoryWalkFrame {
    path: PathBuf,
    cursor: DirectoryCursor,
    snapshot: DirectorySnapshot,
    depth: usize,
}

#[derive(Debug)]
struct VisitedDirectory {
    path: PathBuf,
    snapshot: DirectorySnapshot,
}

#[derive(Default)]
struct DirectoryWalkState {
    stack: Vec<DirectoryWalkFrame>,
    visited: Vec<VisitedDirectory>,
    active: HashSet<(u64, u64)>,
    path_heap_bytes: usize,
}

impl DirectoryWalkState {
    fn new(
        max_depth: usize,
        max_working_bytes: usize,
        path: &Path,
        serve_root: &Path,
    ) -> ListingResult<Self> {
        let active_entries = max_depth.saturating_add(1);
        let active_entry_bytes =
            std::mem::size_of::<(u64, u64)>().saturating_add(std::mem::size_of::<usize>());
        // HashSet::reserve may round its table up. Four slots per requested
        // identity is a deliberately conservative upper bound for hashbrown's
        // load factor and power-of-two bucket rounding.
        let conservative_active_bytes = active_entries
            .saturating_mul(4)
            .saturating_mul(active_entry_bytes);
        if std::mem::size_of::<Self>().saturating_add(conservative_active_bytes) > max_working_bytes
        {
            return Err(walk_working_memory_error(path, serve_root));
        }

        let mut state = Self::default();
        if state.active.try_reserve(active_entries).is_err() {
            return Err(walk_allocation_error(path, serve_root));
        }
        if state.working_bytes(0) > max_working_bytes {
            return Err(walk_working_memory_error(path, serve_root));
        }
        Ok(state)
    }

    fn working_bytes(&self, additional_path_bytes: usize) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.stack
                    .capacity()
                    .saturating_mul(std::mem::size_of::<DirectoryWalkFrame>()),
            )
            .saturating_add(
                self.visited
                    .capacity()
                    .saturating_mul(std::mem::size_of::<VisitedDirectory>()),
            )
            .saturating_add(self.active.capacity().saturating_mul(
                std::mem::size_of::<(u64, u64)>().saturating_add(std::mem::size_of::<usize>()),
            ))
            .saturating_add(self.path_heap_bytes)
            .saturating_add(additional_path_bytes)
    }

    fn push_directory(
        &mut self,
        path: PathBuf,
        snapshot: DirectorySnapshot,
        depth: usize,
        max_depth: usize,
        max_working_bytes: usize,
        serve_root: &Path,
    ) -> ListingResult<()> {
        if depth > max_depth {
            return Err(ListingError::limit(
                "walk_depth",
                &path,
                serve_root,
                "depth_budget",
                ListingProblem::DirectoryDepthLimit,
            ));
        }
        let identity = (snapshot.device, snapshot.inode);
        if self.active.contains(&identity) {
            return Err(ListingError::symlink_loop("walk_loop", &path, serve_root));
        }

        let path_bytes = path.capacity();
        let minimum = std::mem::size_of::<Self>()
            .saturating_add(
                self.stack
                    .len()
                    .saturating_add(1)
                    .saturating_mul(std::mem::size_of::<DirectoryWalkFrame>()),
            )
            .saturating_add(
                self.visited
                    .len()
                    .saturating_mul(std::mem::size_of::<VisitedDirectory>()),
            )
            .saturating_add(self.active.len().saturating_add(1).saturating_mul(
                std::mem::size_of::<(u64, u64)>().saturating_add(std::mem::size_of::<usize>()),
            ))
            .saturating_add(self.path_heap_bytes)
            .saturating_add(path_bytes);
        if minimum > max_working_bytes {
            return Err(walk_working_memory_error(&path, serve_root));
        }
        let stack_allocation = self
            .stack
            .capacity()
            .saturating_mul(std::mem::size_of::<DirectoryWalkFrame>());
        let non_stack_bytes = self
            .working_bytes(path_bytes)
            .saturating_sub(stack_allocation);
        match try_reserve_bounded_vec_slot(&mut self.stack, non_stack_bytes, max_working_bytes) {
            Ok(()) => {}
            Err(BoundedReserveError::Budget) => {
                return Err(walk_working_memory_error(&path, serve_root));
            }
            Err(BoundedReserveError::Allocation) => {
                return Err(walk_allocation_error(&path, serve_root));
            }
        }
        debug_assert!(self.active.capacity() > self.active.len());
        if self.working_bytes(path_bytes) > max_working_bytes {
            return Err(walk_working_memory_error(&path, serve_root));
        }

        self.path_heap_bytes = self.path_heap_bytes.saturating_add(path_bytes);
        self.active.insert(identity);
        self.stack.push(DirectoryWalkFrame {
            path,
            cursor: DirectoryCursor::default(),
            snapshot,
            depth,
        });
        Ok(())
    }

    fn finish_current(&mut self, max_working_bytes: usize, serve_root: &Path) -> ListingResult<()> {
        let frame = self
            .stack
            .pop()
            .expect("a completed directory walk must have an active frame");
        let identity = (frame.snapshot.device, frame.snapshot.inode);
        self.active.remove(&identity);
        let minimum = std::mem::size_of::<Self>()
            .saturating_add(
                self.stack
                    .len()
                    .saturating_mul(std::mem::size_of::<DirectoryWalkFrame>()),
            )
            .saturating_add(
                self.visited
                    .len()
                    .saturating_add(1)
                    .saturating_mul(std::mem::size_of::<VisitedDirectory>()),
            )
            .saturating_add(self.active.len().saturating_mul(
                std::mem::size_of::<(u64, u64)>().saturating_add(std::mem::size_of::<usize>()),
            ))
            .saturating_add(self.path_heap_bytes);
        if minimum > max_working_bytes {
            return Err(walk_working_memory_error(&frame.path, serve_root));
        }
        let visited_allocation = self
            .visited
            .capacity()
            .saturating_mul(std::mem::size_of::<VisitedDirectory>());
        let non_visited_bytes = self.working_bytes(0).saturating_sub(visited_allocation);
        match try_reserve_bounded_vec_slot(&mut self.visited, non_visited_bytes, max_working_bytes)
        {
            Ok(()) => {}
            Err(BoundedReserveError::Budget) => {
                return Err(walk_working_memory_error(&frame.path, serve_root));
            }
            Err(BoundedReserveError::Allocation) => {
                return Err(walk_allocation_error(&frame.path, serve_root));
            }
        }
        if self.working_bytes(0) > max_working_bytes {
            return Err(walk_working_memory_error(&frame.path, serve_root));
        }
        self.visited.push(VisitedDirectory {
            path: frame.path,
            snapshot: frame.snapshot,
        });
        Ok(())
    }
}

fn walk_allocation_error(path: &Path, serve_root: &Path) -> ListingError {
    ListingError::limit(
        "walk_memory",
        path,
        serve_root,
        "working_allocation_failed",
        ListingProblem::DirectoryMemoryLimit,
    )
}

fn walk_working_memory_error(path: &Path, serve_root: &Path) -> ListingError {
    ListingError::limit(
        "walk_memory",
        path,
        serve_root,
        "working_memory_budget",
        ListingProblem::DirectoryMemoryLimit,
    )
}

fn ensure_directory_snapshot_blocking(
    rooted_fs: &RootedFs,
    directory: &VisitedDirectory,
    serve_root: &Path,
    operation: &'static str,
) -> ListingResult<()> {
    let metadata = rooted_fs
        .metadata_blocking(&directory.path)
        .map_err(|error| ListingError::io(operation, &directory.path, serve_root, &error))?;
    if !metadata.is_dir() {
        return Err(ListingError::limit(
            operation,
            &directory.path,
            serve_root,
            "directory_replaced_by_non_directory",
            ListingProblem::DirectoryChanged,
        ));
    }
    if DirectorySnapshot::from_metadata(&metadata) != directory.snapshot {
        return Err(ListingError::limit(
            operation,
            &directory.path,
            serve_root,
            "directory_snapshot_changed",
            ListingProblem::DirectoryChanged,
        ));
    }
    Ok(())
}

pub(in crate::server) async fn collect_dir_items<F, M, W, T>(
    walk: DirectoryWalk,
    include_entry: F,
    mut map_entry: M,
    item_heap_bytes: W,
    byte_budget: Option<CollectionByteBudget>,
) -> ListingResult<Vec<T>>
where
    F: Fn(&RootedDirEntry, &str) -> ListingResult<bool> + Send + 'static,
    M: FnMut(RootedDirEntry) -> ListingResult<T> + Send + 'static,
    W: Fn(&T) -> usize + Send + 'static,
    T: Send + 'static,
{
    let DirectoryWalk {
        work_tasks,
        running,
        cancellation,
        path,
        serve_path,
        rooted_fs,
        max_entries,
        max_depth,
        max_working_bytes,
        max_duration,
        permit,
    } = walk;
    let walk_path = path.clone();
    let walk_root = serve_path.clone();
    let error_path = path;
    let error_root = serve_path;
    let started = Instant::now();
    let worker = spawn_directory_blocking(&work_tasks, permit, move || {
        let root_metadata = match rooted_fs.metadata_blocking(&walk_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err(ListingError::io(
                    "walk_root",
                    &walk_path,
                    &walk_root,
                    &error,
                ));
            }
        };
        if !root_metadata.is_dir() {
            return Err(ListingError::limit(
                "walk_root",
                &walk_path,
                &walk_root,
                "directory_replaced_by_non_directory",
                ListingProblem::DirectoryChanged,
            ));
        }
        let mut state =
            DirectoryWalkState::new(max_depth, max_working_bytes, &walk_path, &walk_root)?;
        state.push_directory(
            walk_path.clone(),
            DirectorySnapshot::from_metadata(&root_metadata),
            0,
            max_depth,
            max_working_bytes,
            &walk_root,
        )?;
        let mut visited = 0usize;
        let mut items = Vec::new();
        let mut heap_bytes = 0usize;

        while !state.stack.is_empty() {
            let frame = state
                .stack
                .last()
                .expect("a non-empty directory stack must have a frame");
            let directory = frame.path.clone();
            let cursor = frame.cursor;
            let current_depth = frame.depth;
            if cancellation.is_cancelled() || !running.load(atomic::Ordering::SeqCst) {
                return Err(ListingError::cancelled(
                    "walk_cancelled",
                    &directory,
                    &walk_root,
                ));
            }
            if started.elapsed() >= max_duration {
                return Err(ListingError::limit(
                    "walk_timeout",
                    &directory,
                    &walk_root,
                    "time_budget",
                    ListingProblem::DirectoryOperationTimeout,
                ));
            }
            ensure_directory_snapshot_blocking(
                &rooted_fs,
                &VisitedDirectory {
                    path: directory.clone(),
                    snapshot: frame.snapshot,
                },
                &walk_root,
                "walk_snapshot_before",
            )?;
            let stopped = std::cell::Cell::new(false);
            let listing_error = std::cell::RefCell::new(None);
            let descend = std::cell::RefCell::new(None);
            let visit_result = rooted_fs.visit_dir_blocking_chunk(
                &directory,
                cursor,
                |entry_path| {
                    if cancellation.is_cancelled() || !running.load(atomic::Ordering::SeqCst) {
                        stopped.set(true);
                        *listing_error.borrow_mut() = Some(ListingError::cancelled(
                            "walk_cancelled",
                            entry_path,
                            &walk_root,
                        ));
                        return false;
                    }
                    if started.elapsed() >= max_duration {
                        stopped.set(true);
                        *listing_error.borrow_mut() = Some(ListingError::limit(
                            "walk_timeout",
                            entry_path,
                            &walk_root,
                            "time_budget",
                            ListingProblem::DirectoryOperationTimeout,
                        ));
                        return false;
                    }
                    visited = visited.saturating_add(1);
                    if visited > max_entries {
                        stopped.set(true);
                        *listing_error.borrow_mut() = Some(ListingError::limit(
                            "walk_limit",
                            entry_path,
                            &walk_root,
                            "entry_budget",
                            ListingProblem::DirectoryWalkEntryLimit,
                        ));
                        return false;
                    }
                    true
                },
                |entry| {
                    // Metadata resolution can itself be slow. Recheck the
                    // cancellation and deadline before using the resolved
                    // entry or descending into it.
                    if cancellation.is_cancelled() || !running.load(atomic::Ordering::SeqCst) {
                        stopped.set(true);
                        *listing_error.borrow_mut() = Some(ListingError::cancelled(
                            "walk_cancelled",
                            &entry.path,
                            &walk_root,
                        ));
                        return Ok(false);
                    }
                    if started.elapsed() >= max_duration {
                        stopped.set(true);
                        *listing_error.borrow_mut() = Some(ListingError::limit(
                            "walk_timeout",
                            &entry.path,
                            &walk_root,
                            "time_budget",
                            ListingProblem::DirectoryOperationTimeout,
                        ));
                        return Ok(false);
                    }
                    let Some(base_name) = entry.file_name.to_str() else {
                        stopped.set(true);
                        *listing_error.borrow_mut() = Some(ListingError::unsupported_name(
                            "walk_filename",
                            &entry.path,
                            &walk_root,
                        ));
                        return Ok(false);
                    };
                    let is_dir = entry.metadata.is_dir();
                    if is_internal_name(base_name) {
                        return Ok(true);
                    }
                    let include = match include_entry(&entry, base_name) {
                        Ok(include) => include,
                        Err(error) => {
                            stopped.set(true);
                            *listing_error.borrow_mut() = Some(error);
                            return Ok(false);
                        }
                    };
                    let child = if is_dir {
                        Some((
                            entry.path.clone(),
                            DirectorySnapshot::from_metadata(&entry.metadata),
                            current_depth.saturating_add(1),
                        ))
                    } else {
                        None
                    };
                    if include {
                        let item_path = entry.path.clone();
                        let item = match map_entry(entry) {
                            Ok(item) => item,
                            Err(error) => {
                                stopped.set(true);
                                *listing_error.borrow_mut() = Some(error);
                                return Ok(false);
                            }
                        };
                        let item_heap_weight = item_heap_bytes(&item);
                        if let Some(budget) = byte_budget {
                            let proposed_heap_bytes = heap_bytes.saturating_add(item_heap_weight);
                            let minimum = collection_allocation_bytes(
                                items.len().saturating_add(1),
                                std::mem::size_of::<T>(),
                                proposed_heap_bytes,
                            );
                            if minimum > budget.max_bytes {
                                stopped.set(true);
                                *listing_error.borrow_mut() = Some(ListingError::limit(
                                    budget.operation,
                                    &item_path,
                                    &walk_root,
                                    budget.reason,
                                    budget.problem,
                                ));
                                return Ok(false);
                            }
                            match try_reserve_bounded_vec_slot(
                                &mut items,
                                std::mem::size_of::<Vec<T>>().saturating_add(proposed_heap_bytes),
                                budget.max_bytes,
                            ) {
                                Ok(()) => {}
                                Err(BoundedReserveError::Budget) => {
                                    stopped.set(true);
                                    *listing_error.borrow_mut() = Some(ListingError::limit(
                                        budget.operation,
                                        &item_path,
                                        &walk_root,
                                        budget.reason,
                                        budget.problem,
                                    ));
                                    return Ok(false);
                                }
                                Err(BoundedReserveError::Allocation) => {
                                    stopped.set(true);
                                    *listing_error.borrow_mut() = Some(ListingError::limit(
                                        budget.operation,
                                        &item_path,
                                        &walk_root,
                                        "result_allocation_failed",
                                        budget.allocation_problem,
                                    ));
                                    return Ok(false);
                                }
                            }
                            let allocated = std::mem::size_of::<Vec<T>>()
                                .saturating_add(
                                    items.capacity().saturating_mul(std::mem::size_of::<T>()),
                                )
                                .saturating_add(proposed_heap_bytes);
                            if allocated > budget.max_bytes {
                                stopped.set(true);
                                *listing_error.borrow_mut() = Some(ListingError::limit(
                                    budget.operation,
                                    &item_path,
                                    &walk_root,
                                    budget.reason,
                                    budget.problem,
                                ));
                                return Ok(false);
                            }
                        }
                        heap_bytes = heap_bytes.saturating_add(item_heap_weight);
                        items.push(item);
                    }
                    if let Some(child) = child {
                        *descend.borrow_mut() = Some(child);
                        return Ok(false);
                    }
                    Ok(true)
                },
            );
            if stopped.get() {
                return Err(listing_error.into_inner().unwrap_or_else(|| {
                    ListingError::invariant("walk_stopped", &directory, &walk_root)
                }));
            }
            match visit_result {
                Err(error) => {
                    return Err(ListingError::io(
                        "walk_next",
                        &directory,
                        &walk_root,
                        &error,
                    ));
                }
                Ok(DirectoryVisitProgress::Complete) => {
                    state.finish_current(max_working_bytes, &walk_root)?;
                }
                Ok(DirectoryVisitProgress::Paused(cursor)) => {
                    state
                        .stack
                        .last_mut()
                        .expect("a paused directory must remain active")
                        .cursor = cursor;
                    let Some((child_path, child_snapshot, child_depth)) = descend.into_inner()
                    else {
                        return Err(ListingError::invariant(
                            "walk_paused_without_child",
                            &directory,
                            &walk_root,
                        ));
                    };
                    state.push_directory(
                        child_path,
                        child_snapshot,
                        child_depth,
                        max_depth,
                        max_working_bytes,
                        &walk_root,
                    )?;
                }
            }
        }

        for directory in &state.visited {
            if cancellation.is_cancelled() || !running.load(atomic::Ordering::SeqCst) {
                return Err(ListingError::cancelled(
                    "walk_snapshot_after",
                    &directory.path,
                    &walk_root,
                ));
            }
            if started.elapsed() >= max_duration {
                return Err(ListingError::limit(
                    "walk_snapshot_after",
                    &directory.path,
                    &walk_root,
                    "time_budget",
                    ListingProblem::DirectoryOperationTimeout,
                ));
            }
            ensure_directory_snapshot_blocking(
                &rooted_fs,
                directory,
                &walk_root,
                "walk_snapshot_after",
            )?;
        }
        Ok(items)
    });

    worker.await.map_err(|error| ListingError {
        operation: "walk_worker",
        relative_path: safe_relative_path(&error_path, &error_root),
        reason: format!("worker_join_error={error}"),
        problem: ListingProblem::DirectoryOperationFailed,
    })?
}

#[cfg(test)]
pub(in crate::server) async fn collect_dir_entries<F>(
    walk: DirectoryWalk,
    include_entry: F,
) -> ListingResult<Vec<RootedDirEntry>>
where
    F: Fn(&RootedDirEntry, &str) -> ListingResult<bool> + Send + 'static,
{
    collect_dir_items(walk, include_entry, Ok, |_| 0, None).await
}
