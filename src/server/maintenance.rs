use super::{
    Server,
    internal_names::{InternalEntryName, classify_internal_name, upload_temp_path},
    rooted_fs::{
        DirectoryCursor, DirectoryVisitProgress, RootedEntryKey, RootedFs, TrashEntry,
        TrashPurgeProgress,
    },
    state_store::{StateStore, StoredFileIdentity, StoredUploadSession, StoredUploadState},
};
use crate::server::blocking_io::blocking_io_gate;

use anyhow::{Context, Result, anyhow, ensure};
use std::{
    collections::HashSet,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(in crate::server) const UPLOAD_SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const TRASH_ENTRY_TTL: Duration = Duration::from_secs(60 * 60);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAINTENANCE_CONTINUATION_DELAY: Duration = Duration::from_millis(25);
const MAINTENANCE_ENTRY_BUDGET: usize = 1024;
const MAINTENANCE_TIME_BUDGET: Duration = Duration::from_millis(100);
const UPLOAD_SESSION_CLEANUP_BATCH: usize = 64;
const PURGE_JOB_SNAPSHOT_LIMIT: usize = 4096;

pub(super) fn claim_changes() -> &'static watch::Sender<u64> {
    static CLAIM_EPOCH: OnceLock<watch::Sender<u64>> = OnceLock::new();
    CLAIM_EPOCH.get_or_init(|| {
        let (changes, _) = watch::channel(0);
        changes
    })
}

fn notify_claim_change() {
    claim_changes().send_modify(|epoch| *epoch = epoch.wrapping_add(1));
}

pub(super) struct MaintenanceScanState {
    pub(super) directories: Vec<MaintenanceDirectory>,
    pub(super) pending_purge: Option<TrashEntry>,
    trash_ttl: Duration,
    upload_session_cleanup_complete: bool,
    protected_upload_entries: HashSet<RootedEntryKey>,
    skip_untracked_upload_cleanup: bool,
    purge_job_snapshot_complete: bool,
    protected_trash_entries: HashSet<RootedEntryKey>,
    skip_untracked_trash_cleanup: bool,
}

pub(super) struct MaintenanceDirectory {
    path: PathBuf,
    pub(super) cursor: DirectoryCursor,
}

#[derive(Clone, Copy)]
pub(super) struct MaintenanceBudget {
    pub(super) max_entries: usize,
    pub(super) max_duration: Duration,
}

#[derive(Clone, Copy)]
pub(super) struct MaintenanceBatchOptions {
    pub(super) now: SystemTime,
    pub(super) upload_ttl: Duration,
    pub(super) budget: MaintenanceBudget,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct UploadSessionCleanupBatch {
    pub(super) examined: usize,
    pub(super) removed_records: usize,
    pub(super) removed_stages: usize,
    protected_entries: Vec<RootedEntryKey>,
    skip_untracked_upload_cleanup: bool,
}

impl UploadSessionCleanupBatch {
    fn should_continue(&self, limit: usize) -> bool {
        self.examined == limit && self.removed_records != 0
    }
}

struct MaintenanceCleanupClaim<'a> {
    active: &'a Mutex<HashSet<RootedEntryKey>>,
    marker: RootedEntryKey,
}

impl Drop for MaintenanceCleanupClaim<'_> {
    fn drop(&mut self) {
        let removed = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.marker);
        if removed {
            notify_claim_change();
        }
    }
}

fn try_claim_stale_entry<'a>(
    active: &'a Mutex<HashSet<RootedEntryKey>>,
    entry: &RootedEntryKey,
) -> Option<MaintenanceCleanupClaim<'a>> {
    let marker = entry.maintenance_marker();
    let mut active_entries = active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if active_entries.contains(entry) || active_entries.contains(&marker) {
        return None;
    }
    active_entries.insert(marker.clone());
    drop(active_entries);
    Some(MaintenanceCleanupClaim { active, marker })
}

impl MaintenanceScanState {
    pub(super) fn new(root: PathBuf, trash_ttl: Duration) -> Self {
        Self {
            directories: vec![MaintenanceDirectory {
                path: root,
                cursor: DirectoryCursor::default(),
            }],
            pending_purge: None,
            trash_ttl,
            upload_session_cleanup_complete: false,
            protected_upload_entries: HashSet::new(),
            skip_untracked_upload_cleanup: false,
            purge_job_snapshot_complete: false,
            protected_trash_entries: HashSet::new(),
            skip_untracked_trash_cleanup: false,
        }
    }
}

impl Server {
    pub(in crate::server) async fn run_storage_maintenance(self: Arc<Self>) {
        let mut scan =
            MaintenanceScanState::new(self.content.args.serve_path.clone(), TRASH_ENTRY_TTL);
        loop {
            let (next, complete) = match self.cleanup_stale_internal_files(scan).await {
                Ok(result) => result,
                Err(error) => {
                    warn!("Failed to clean stale internal files error={error:#}");
                    scan = MaintenanceScanState::new(
                        self.content.args.serve_path.clone(),
                        TRASH_ENTRY_TTL,
                    );
                    tokio::select! {
                        _ = self.lifecycle.shutdown.cancelled() => return,
                        _ = tokio::time::sleep(MAINTENANCE_CONTINUATION_DELAY) => {}
                    }
                    continue;
                }
            };
            scan = next;
            if complete {
                tokio::select! {
                    _ = self.lifecycle.shutdown.cancelled() => return,
                    _ = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
                }
                scan = MaintenanceScanState::new(
                    self.content.args.serve_path.clone(),
                    TRASH_ENTRY_TTL,
                );
            } else {
                tokio::select! {
                    _ = self.lifecycle.shutdown.cancelled() => return,
                    _ = tokio::time::sleep(MAINTENANCE_CONTINUATION_DELAY) => {}
                }
            }
        }
    }

    async fn cleanup_stale_internal_files(
        &self,
        mut state: MaintenanceScanState,
    ) -> Result<(MaintenanceScanState, bool)> {
        if !state.upload_session_cleanup_complete {
            match cleanup_expired_upload_sessions_batch(
                &self.content.rooted_fs,
                &self.state.state_store,
                &self.admission.active_upload_files,
                &self.lifecycle.shutdown,
                UPLOAD_SESSION_CLEANUP_BATCH,
            )
            .await
            {
                Ok(batch) => {
                    state.upload_session_cleanup_complete =
                        !batch.should_continue(UPLOAD_SESSION_CLEANUP_BATCH);
                    state
                        .protected_upload_entries
                        .extend(batch.protected_entries.iter().cloned());
                    state.skip_untracked_upload_cleanup |= batch.skip_untracked_upload_cleanup
                        || (batch.examined == UPLOAD_SESSION_CLEANUP_BATCH
                            && batch.removed_records == 0);
                    if batch.removed_records != 0 {
                        info!(
                            "Removed expired upload session records={} stages={}",
                            batch.removed_records, batch.removed_stages
                        );
                    }
                }
                Err(error) => {
                    // A broken control-plane query must not create a 25 ms
                    // retry loop. The next hourly scan retries it while the
                    // independent orphan-file scan continues now.
                    warn!("Failed to clean expired upload sessions error={error:#}");
                    state.upload_session_cleanup_complete = true;
                    state.skip_untracked_upload_cleanup = true;
                }
            }
        }
        if !state.upload_session_cleanup_complete {
            // Drain or deliberately stop the bounded SQLite pages before the
            // orphan scan. Otherwise a later unexamined row could have
            // its staging pathname deleted without the recorded inode/owner
            // checks above.
            return Ok((state, false));
        }
        if !state.purge_job_snapshot_complete {
            load_tracked_purge_snapshot(
                &self.content.rooted_fs,
                &self.state.state_store,
                &mut state,
            )
            .await;
        }
        let rooted_fs = self.content.rooted_fs.clone();
        let active = self.admission.active_upload_files.clone();
        let shutdown = self.lifecycle.shutdown.clone();
        let purge_queue = self.state.purge_queue.clone();
        let gate = blocking_io_gate().clone();
        let (state, removed, _, complete, _) = self
            .lifecycle
            .work_tasks
            .spawn(async move {
                gate.run(move || {
                    collect_stale_internal_files_batch(
                        &rooted_fs,
                        state,
                        &active,
                        MaintenanceBatchOptions {
                            now: SystemTime::now(),
                            upload_ttl: UPLOAD_SESSION_TTL,
                            budget: MaintenanceBudget {
                                max_entries: MAINTENANCE_ENTRY_BUDGET,
                                max_duration: MAINTENANCE_TIME_BUDGET,
                            },
                        },
                        &shutdown,
                        |entry| purge_queue.try_schedule(entry),
                    )
                })
                .await
            })
            .await??;

        for path in removed {
            match self.content.rooted_fs.sync_parent(&path).await {
                Ok(()) => info!("Removed stale internal file path={}", path.display()),
                Err(error) => warn!(
                    "Failed to sync a removed stale internal file parent path={} error={error:#}",
                    path.display()
                ),
            }
        }
        let complete = complete && state.upload_session_cleanup_complete;
        Ok((state, complete))
    }
}

async fn load_tracked_purge_snapshot(
    rooted_fs: &RootedFs,
    state_store: &StateStore,
    state: &mut MaintenanceScanState,
) {
    match tracked_purge_trash_entries(rooted_fs, state_store).await {
        Ok(entries) => state.protected_trash_entries = entries,
        Err(error) => {
            // Fail closed for only the orphan-trash path. The durable
            // purge worker and upload-stage cleanup remain independent and can
            // continue making progress.
            warn!("Failed to snapshot tracked purge trash error={error:#}");
            state.skip_untracked_trash_cleanup = true;
        }
    }
    state.purge_job_snapshot_complete = true;
}

async fn tracked_purge_trash_entries(
    rooted_fs: &RootedFs,
    state_store: &StateStore,
) -> Result<HashSet<RootedEntryKey>> {
    let jobs = state_store.purge_jobs(PURGE_JOB_SNAPSHOT_LIMIT).await?;
    let mut entries = HashSet::with_capacity(jobs.len());
    for job in jobs {
        let path = rooted_fs
            .resolve_state_path(&job.trash_path)
            .with_context(|| {
                format!(
                    "invalid tracked purge trash path for job {}",
                    uuid::Uuid::from_bytes(job.key.id)
                )
            })?;
        ensure!(
            rooted_fs.state_relative_path(&path)? == job.trash_path,
            "tracked purge trash path is not canonical"
        );
        ensure!(
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(classify_internal_name)
                == Some(InternalEntryName::DeleteTrash),
            "tracked purge trash path is not a canonical delete-trash name"
        );
        entries.insert(
            rooted_fs
                .entry_key(&path)
                .await
                .with_context(|| format!("failed to key tracked purge trash {}", path.display()))?,
        );
    }
    Ok(entries)
}

/// Reclaim one bounded page of expired SQLite upload sessions.
///
/// Database paths are treated as hostile input. A running stage
/// is unlinked only through an fd-relative purge capability whose inode still
/// matches the identity durably recorded at the last checkpoint. Unknown and
/// terminal rows expire without touching either the target or a possibly
/// renamed stage; an in-flight `CommitStarted` row remains an ambiguity
/// barrier until restart recovery or the commit task advances it.
pub(super) async fn cleanup_expired_upload_sessions_batch(
    rooted_fs: &RootedFs,
    state_store: &StateStore,
    active: &Mutex<HashSet<RootedEntryKey>>,
    shutdown: &CancellationToken,
    limit: usize,
) -> Result<UploadSessionCleanupBatch> {
    let sessions = state_store.expired_upload_sessions(limit).await?;
    let mut batch = UploadSessionCleanupBatch::default();
    for session in sessions {
        if shutdown.is_cancelled() {
            break;
        }
        batch.examined += 1;
        match cleanup_expired_upload_session(
            rooted_fs,
            state_store,
            active,
            shutdown,
            &session,
            &mut batch,
        )
        .await
        {
            Ok(Some(stage_removed)) => {
                batch.removed_records += 1;
                batch.removed_stages += usize::from(stage_removed);
            }
            Ok(None) => {}
            Err(error) => {
                // One inaccessible or concurrently changing artifact must not
                // prevent later rows in this bounded page from being visited.
                batch.skip_untracked_upload_cleanup = true;
                warn!(
                    "Failed to clean expired upload session id={} error={error:#}",
                    uuid::Uuid::from_bytes(session.key.id)
                );
            }
        }
    }
    Ok(batch)
}

async fn cleanup_expired_upload_session(
    rooted_fs: &RootedFs,
    state_store: &StateStore,
    active: &Mutex<HashSet<RootedEntryKey>>,
    shutdown: &CancellationToken,
    expired: &StoredUploadSession,
    batch: &mut UploadSessionCleanupBatch,
) -> Result<Option<bool>> {
    let upload_id = uuid::Uuid::from_bytes(expired.key.id);
    let target_path = match rooted_fs.resolve_state_path(&expired.target_path) {
        Ok(path) => path,
        Err(error) => {
            batch.skip_untracked_upload_cleanup = true;
            warn!(
                "Discarding expired upload session with an invalid target path id={upload_id} error={error}"
            );
            return remove_expired_upload_record(state_store, expired, false).await;
        }
    };
    let stage_path = match rooted_fs.resolve_state_path(&expired.stage_path) {
        Ok(path) => path,
        Err(error) => {
            batch.skip_untracked_upload_cleanup = true;
            warn!(
                "Discarding expired upload session with an invalid stage path id={upload_id} error={error}"
            );
            return remove_expired_upload_record(state_store, expired, false).await;
        }
    };
    let canonical_stage = match upload_temp_path(&target_path, upload_id) {
        Ok(path) => path,
        Err(error) => {
            batch.skip_untracked_upload_cleanup = true;
            warn!(
                "Discarding expired upload session with an invalid target mapping id={upload_id} error={error:#}"
            );
            return remove_expired_upload_record(state_store, expired, false).await;
        }
    };
    if canonical_stage != stage_path
        || rooted_fs.state_relative_path(&target_path)? != expired.target_path
        || rooted_fs.state_relative_path(&stage_path)? != expired.stage_path
    {
        batch.skip_untracked_upload_cleanup = true;
        warn!("Discarding expired upload session with mismatched rooted paths id={upload_id}");
        return remove_expired_upload_record(state_store, expired, false).await;
    }

    // Paths and state are immutable for every transition. Recheck the exact
    // expired snapshot before acting, then retire terminal/unknown records
    // without opening the stage at all. A missing, inaccessible, or malicious
    // stage must not pin those rows past their TTL. CommitStarted can still be
    // advanced by an in-flight non-cancellable commit, so it remains an
    // ambiguity barrier if an older database happens to return it here.
    match state_store.upload_session(expired.key).await? {
        Some(current) if current == *expired => {}
        Some(_) | None => return Ok(None),
    }
    match expired.state {
        StoredUploadState::Committed | StoredUploadState::Rejected | StoredUploadState::Unknown => {
            return remove_expired_upload_record(state_store, expired, false).await;
        }
        StoredUploadState::CommitStarted => return Ok(None),
        StoredUploadState::Running | StoredUploadState::AwaitingConfirmation => {}
    }

    let stage_key = match rooted_fs.entry_key(&stage_path).await {
        Ok(key) => key,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return remove_expired_upload_record(state_store, expired, false).await;
        }
        Err(error) => return Err(error.into()),
    };
    let Some(_claim) = try_claim_stale_entry(active, &stage_key) else {
        // The upload either still owns the stage or another cleanup owns its
        // marker. Keep the database ambiguity barrier for the next scan.
        return Ok(None);
    };
    batch.protected_entries.push(stage_key.clone());
    // A checkpoint may have advanced between the expiry query and acquiring
    // the maintenance claim. Only act on the exact snapshot that expired.
    match state_store.upload_session(expired.key).await? {
        Some(current) if current == *expired => {}
        Some(_) | None => return Ok(None),
    }

    cleanup_expired_running_upload(rooted_fs, state_store, shutdown, expired, &stage_path).await
}

async fn cleanup_expired_running_upload(
    rooted_fs: &RootedFs,
    state_store: &StateStore,
    shutdown: &CancellationToken,
    expired: &StoredUploadSession,
    stage_path: &Path,
) -> Result<Option<bool>> {
    let Some(expected_stage) = expired.stage_identity else {
        // Without a durable inode identity no filesystem object is safe to
        // unlink. The expired record itself is still bounded by its TTL.
        return remove_expired_upload_record(state_store, expired, false).await;
    };
    let stage = match rooted_fs.capture_entry_for_purge(stage_path, false).await {
        Ok(Some(entry)) if stored_identity_matches(expected_stage, &entry) => Some(entry),
        Ok(Some(_)) => {
            // The name was reused. It is not this upload's stage.
            return remove_expired_upload_record(state_store, expired, false).await;
        }
        Ok(None) => None,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::NotADirectory
                    | std::io::ErrorKind::InvalidData
            ) =>
        {
            return remove_expired_upload_record(state_store, expired, false).await;
        }
        Err(error) => return Err(error.into()),
    };

    let stage_removed = if let Some(stage) = stage {
        purge_single_file(stage, shutdown).await?;
        true
    } else {
        false
    };
    remove_expired_upload_record(state_store, expired, stage_removed).await
}

fn stored_identity_matches(expected: StoredFileIdentity, entry: &TrashEntry) -> bool {
    let actual = entry.identity();
    !actual.is_directory && actual.device == expected.device && actual.inode == expected.inode
}

async fn purge_single_file(entry: TrashEntry, shutdown: &CancellationToken) -> Result<()> {
    match entry
        .purge_slice(1, MAINTENANCE_TIME_BUDGET, shutdown.clone())
        .await
    {
        Ok(TrashPurgeProgress::Complete) => Ok(()),
        Ok(TrashPurgeProgress::Pending(_)) => Err(anyhow!(
            "Upload artifact cleanup did not complete within its bounded slice"
        )),
        Err(error) => {
            let (_, source) = error.into_parts();
            Err(source.into())
        }
    }
}

async fn remove_expired_upload_record(
    state_store: &StateStore,
    expired: &StoredUploadSession,
    stage_removed: bool,
) -> Result<Option<bool>> {
    Ok(state_store
        .remove_upload_session(expired.key)
        .await?
        .then_some(stage_removed))
}

pub(super) fn collect_stale_internal_files_batch<S>(
    rooted_fs: &RootedFs,
    mut state: MaintenanceScanState,
    active: &Mutex<HashSet<RootedEntryKey>>,
    options: MaintenanceBatchOptions,
    shutdown: &CancellationToken,
    mut schedule_purge: S,
) -> (
    MaintenanceScanState,
    Vec<PathBuf>,
    Vec<PathBuf>,
    bool,
    usize,
)
where
    S: FnMut(TrashEntry) -> std::result::Result<(), Box<TrashEntry>>,
{
    let mut removed = Vec::new();
    let mut scheduled = Vec::new();
    let mut examined = 0usize;
    let deadline = StdInstant::now()
        .checked_add(options.budget.max_duration)
        .unwrap_or_else(StdInstant::now);

    if let Some(entry) = state.pending_purge.take() {
        match schedule_purge(entry) {
            Ok(()) => {}
            Err(entry) => {
                // The hidden trash entry remains on disk and a later full scan
                // can capture it again. Do not pin this scan behind a saturated
                // purge queue, because permanently failing admitted jobs must
                // not starve unrelated stale upload cleanup.
                drop(entry);
            }
        }
    }

    while !state.directories.is_empty()
        && examined < options.budget.max_entries
        && StdInstant::now() < deadline
        && !shutdown.is_cancelled()
    {
        let directory_index = state.directories.len() - 1;
        let directory_path = state.directories[directory_index].path.clone();
        let directory_cursor = state.directories[directory_index].cursor;
        let mut descend = None;
        let mut pending_purge = None;
        let progress = rooted_fs.visit_dir_blocking_chunk(
            &directory_path,
            directory_cursor,
            |_| {
                if examined >= options.budget.max_entries
                    || StdInstant::now() >= deadline
                    || shutdown.is_cancelled()
                {
                    return false;
                }
                examined += 1;
                true
            },
            |entry| {
                // Internal files are always regular files or real directories.
                // Never follow a user-created symlink while doing maintenance.
                if entry.is_symlink {
                    return Ok(true);
                }
                let is_dir = entry.metadata.is_dir();
                let Some(name) = entry.file_name.to_str() else {
                    if is_dir {
                        descend = Some(entry.path);
                        return Ok(false);
                    }
                    return Ok(true);
                };
                let Some(internal_name) = classify_internal_name(name) else {
                    if is_dir {
                        descend = Some(entry.path);
                        return Ok(false);
                    }
                    return Ok(true);
                };
                // Quarantine is a fail-safe terminal location for an internal
                // trash entry whose identity could not be reconciled. It is
                // intentionally hidden and never eligible for automatic TTL
                // cleanup; an operator must inspect and remove it explicitly.
                if internal_name == InternalEntryName::Quarantine {
                    return Ok(true);
                }
                let is_trash = internal_name == InternalEntryName::DeleteTrash;
                if is_dir && !is_trash {
                    warn!(
                        "Refusing to recursively remove an invalid upload session directory path={}",
                        entry.path.display()
                    );
                    return Ok(true);
                }
                let age = maintenance_entry_age(&entry.metadata, options.now, is_trash);
                let ttl = if is_trash {
                    state.trash_ttl
                } else {
                    options.upload_ttl
                };
                if age < ttl {
                    return Ok(true);
                }

                if is_trash {
                    if state.skip_untracked_trash_cleanup {
                        return Ok(true);
                    }
                    let entry_key = match rooted_fs.entry_key_blocking(&entry.path) {
                        Ok(key) => key,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound
                                    | std::io::ErrorKind::NotADirectory
                            ) =>
                        {
                            return Ok(true);
                        }
                        Err(error) => return Err(error),
                    };
                    if state.protected_trash_entries.contains(&entry_key) {
                        return Ok(true);
                    }
                    let purge =
                        match rooted_fs.capture_entry_for_purge_blocking(&entry.path, is_dir) {
                            Ok(Some(purge)) => purge,
                            Ok(None) => return Ok(true),
                            Err(error) => {
                                warn!(
                                    "Failed to capture stale trash for purge path={} error={error}",
                                    entry.path.display()
                                );
                                return Ok(true);
                            }
                        };
                    match schedule_purge(purge) {
                        Ok(()) => scheduled.push(entry.path),
                        Err(purge) => {
                            pending_purge = Some(*purge);
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }

                if state.skip_untracked_upload_cleanup
                    && matches!(
                        internal_name,
                        InternalEntryName::Stage | InternalEntryName::State
                    )
                {
                    return Ok(true);
                }

                let entry_key = match rooted_fs.entry_key_blocking(&entry.path) {
                    Ok(key) => key,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return Ok(true);
                    }
                    Err(error) => return Err(error),
                };
                if state.protected_upload_entries.contains(&entry_key) {
                    return Ok(true);
                }
                let Some(_cleanup_claim) = try_claim_stale_entry(active, &entry_key) else {
                    return Ok(true);
                };
                match rooted_fs.remove_entry_blocking(&entry.path, is_dir) {
                    Ok(true) => removed.push(entry.path),
                    Ok(false) => {}
                    Err(error) => {
                        warn!(
                            "Failed to remove stale internal file path={} error={error}",
                            entry.path.display()
                        );
                    }
                }
                Ok(true)
            },
        );

        match progress {
            Ok(DirectoryVisitProgress::Complete) => {
                state.directories.pop();
            }
            Ok(DirectoryVisitProgress::Paused(cursor)) => {
                state.directories[directory_index].cursor = cursor;
            }
            Err(error) => {
                warn!(
                    "Failed to scan internal files in directory path={} error={error}",
                    directory_path.display()
                );
                state.directories.pop();
            }
        }

        if let Some(purge) = pending_purge {
            state.pending_purge = Some(purge);
            break;
        }
        if let Some(path) = descend {
            state.directories.push(MaintenanceDirectory {
                path,
                cursor: DirectoryCursor::default(),
            });
        }
    }
    let complete = state.directories.is_empty() && state.pending_purge.is_none();
    (state, removed, scheduled, complete, examined)
}

fn maintenance_entry_age(
    metadata: &std::fs::Metadata,
    now: SystemTime,
    is_trash: bool,
) -> Duration {
    let mut newest_change = metadata.modified().ok();
    if is_trash
        && let (Ok(seconds), Ok(nanoseconds)) = (
            u64::try_from(metadata.ctime()),
            u32::try_from(metadata.ctime_nsec()),
        )
        && nanoseconds < 1_000_000_000
        && let Some(changed) = UNIX_EPOCH.checked_add(Duration::new(seconds, nanoseconds))
    {
        newest_change = Some(
            newest_change
                .map(|modified| modified.max(changed))
                .unwrap_or(changed),
        );
    }
    newest_change
        .and_then(|changed| now.duration_since(changed).ok())
        .unwrap_or_default()
}

#[cfg(test)]
pub(super) fn collect_and_remove_stale_internal_files(
    rooted_fs: &RootedFs,
    root: &Path,
    active: &Mutex<HashSet<RootedEntryKey>>,
    now: SystemTime,
    upload_ttl: Duration,
    trash_ttl: Duration,
    shutdown: Option<&CancellationToken>,
) -> Result<Vec<PathBuf>> {
    let fallback_shutdown = CancellationToken::new();
    let shutdown = shutdown.unwrap_or(&fallback_shutdown);
    let mut state = MaintenanceScanState::new(root.to_path_buf(), trash_ttl);
    let mut all_removed = Vec::new();
    loop {
        let (next, removed, scheduled, complete, _) = collect_stale_internal_files_batch(
            rooted_fs,
            state,
            active,
            MaintenanceBatchOptions {
                now,
                upload_ttl,
                budget: MaintenanceBudget {
                    max_entries: usize::MAX,
                    max_duration: Duration::from_secs(60),
                },
            },
            shutdown,
            |entry| {
                if let Err(error) = entry.purge_all_blocking() {
                    warn!("Failed to purge test trash entry error={error}");
                }
                Ok(())
            },
        );
        state = next;
        all_removed.extend(removed);
        all_removed.extend(scheduled);
        if complete || shutdown.is_cancelled() {
            return Ok(all_removed);
        }
    }
}

#[cfg(test)]
mod tests;
