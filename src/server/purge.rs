use super::{
    Server,
    identity::OwnerId,
    rooted_fs::{DeleteIdentity, TrashEntry, TrashPurgeProgress},
    state_store::{
        PurgeJobKey, StorePurgeJob, StoredFileIdentity, StoredPurgeJob, StoredPurgeState,
    },
};

use anyhow::{Result, anyhow};
use std::{collections::VecDeque, path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

const DELETE_PURGE_SIGNAL_CAPACITY: usize = 64;
const DELETE_PURGE_ACTIVE_CAPACITY: usize = 64;
const DELETE_PURGE_RECONCILE_LIMIT: usize = 4096;
const DELETE_PURGE_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const DELETE_PURGE_IDLE_POLL: Duration = Duration::from_secs(1);
pub(in crate::server) const DELETE_PURGE_SLICE_ENTRIES: usize = 256;
pub(in crate::server) const DELETE_PURGE_SLICE_TIME: Duration = Duration::from_millis(25);
pub(in crate::server) const DELETE_PURGE_RETRY_BASE: Duration = Duration::from_millis(100);
pub(in crate::server) const DELETE_PURGE_RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(in crate::server) struct PurgeQueue {
    sender: mpsc::Sender<PurgeSignal>,
}

pub(in crate::server) enum PurgeSignal {
    Wake,
    Orphan(TrashEntry),
}

enum PurgeTask {
    Durable(PurgeWork),
    Orphan(TrashEntry),
}

pub(in crate::server) struct PreparedPurge {
    pub(in crate::server) key: PurgeJobKey,
    pub(in crate::server) trash_id: Uuid,
    pub(in crate::server) source_identity: DeleteIdentity,
}

pub(in crate::server) enum PreparePurge {
    Prepared(PreparedPurge),
    Full,
}

pub(in crate::server) struct PurgeWork {
    job: StoredPurgeJob,
    entry: TrashEntry,
}

#[derive(Debug)]
struct ClaimedPurgeFailure {
    // Keep the retry evidence intact without making every Result carrying
    // this error reserve space for the full durable job record.
    job: Box<StoredPurgeJob>,
    source: anyhow::Error,
}

impl ClaimedPurgeFailure {
    fn new(job: StoredPurgeJob, source: impl Into<anyhow::Error>) -> Self {
        Self {
            job: Box::new(job),
            source: source.into(),
        }
    }
}

enum PurgeWorkResult {
    Complete,
    Pending(Box<PurgeWork>),
    Retry(ClaimedPurgeFailure),
}

struct ClaimedPurgeRetry {
    due: Instant,
    job: StoredPurgeJob,
    local_failures: u32,
}

impl PurgeQueue {
    pub(in crate::server) fn new() -> (Self, mpsc::Receiver<PurgeSignal>) {
        let (sender, receiver) = mpsc::channel(DELETE_PURGE_SIGNAL_CAPACITY);
        (Self { sender }, receiver)
    }

    /// Wakeups are deliberately coalesced. SQLite, rather than this channel,
    /// is the durable queue and will be polled again even if the wake slot is
    /// already occupied.
    pub(in crate::server) fn notify(&self) {
        let _ = self.sender.try_send(PurgeSignal::Wake);
    }

    /// Old, unjournaled trash can still be discovered by the low-frequency
    /// filesystem reconciler. It is safe to use a bounded in-memory handoff
    /// for these orphans: a saturated channel leaves the entry hidden and the
    /// next scan will rediscover it. New deletes never depend on this path.
    pub(in crate::server) fn try_schedule(
        &self,
        entry: TrashEntry,
    ) -> std::result::Result<(), Box<TrashEntry>> {
        match self.sender.try_send(PurgeSignal::Orphan(entry)) {
            Ok(()) => Ok(()),
            Err(
                mpsc::error::TrySendError::Full(PurgeSignal::Orphan(entry))
                | mpsc::error::TrySendError::Closed(PurgeSignal::Orphan(entry)),
            ) => Err(Box::new(entry)),
            Err(
                mpsc::error::TrySendError::Full(PurgeSignal::Wake)
                | mpsc::error::TrySendError::Closed(PurgeSignal::Wake),
            ) => {
                unreachable!("the submitted signal contains an orphan entry")
            }
        }
    }
}

impl Server {
    #[cfg(test)]
    pub(in crate::server) async fn prepare_purge(
        &self,
        owner: &str,
        target: &Path,
    ) -> Result<PreparePurge> {
        let source_identity = self.content.rooted_fs.delete_identity(target).await?;
        self.prepare_purge_with_identity(owner, target, source_identity)
            .await
    }

    pub(in crate::server) async fn prepare_purge_with_identity(
        &self,
        owner: &str,
        target: &Path,
        source_identity: DeleteIdentity,
    ) -> Result<PreparePurge> {
        let target_path = self.content.rooted_fs.state_relative_path(target)?;
        let owner = OwnerId::persistent(owner).into_bytes();

        for _ in 0..16 {
            let trash_id = Uuid::new_v4();
            let trash_path = self.content.rooted_fs.trash_path_for_id(target, trash_id)?;
            let trash_path = self.content.rooted_fs.state_relative_path(&trash_path)?;
            let key = PurgeJobKey {
                owner,
                id: trash_id.into_bytes(),
            };
            let job = StoredPurgeJob {
                key,
                target_path: target_path.clone(),
                trash_path,
                source_identity: StoredFileIdentity {
                    device: source_identity.device,
                    inode: source_identity.inode,
                },
                trash_revision: None,
                is_directory: source_identity.is_directory,
                state: StoredPurgeState::Prepared,
                attempts: 0,
            };
            match self.state.state_store.prepare_purge_job(job).await? {
                StorePurgeJob::Inserted => {
                    return Ok(PreparePurge::Prepared(PreparedPurge {
                        key,
                        trash_id,
                        source_identity,
                    }));
                }
                StorePurgeJob::Existing | StorePurgeJob::Conflict => continue,
                StorePurgeJob::Full => return Ok(PreparePurge::Full),
            }
        }
        Err(anyhow!(
            "failed to allocate a unique durable purge identifier"
        ))
    }

    pub(in crate::server) fn notify_purge_worker(&self) {
        self.state.purge_queue.notify();
    }

    pub(in crate::server) async fn run_purge_worker(
        self: Arc<Self>,
        mut receiver: mpsc::Receiver<PurgeSignal>,
    ) {
        let mut pending = VecDeque::<PurgeTask>::new();
        let mut claimed_retries = VecDeque::<ClaimedPurgeRetry>::new();
        loop {
            if self.lifecycle.shutdown.is_cancelled() {
                return;
            }

            while pending.len() + claimed_retries.len() < DELETE_PURGE_ACTIVE_CAPACITY {
                match receiver.try_recv() {
                    Ok(PurgeSignal::Wake) => continue,
                    Ok(PurgeSignal::Orphan(entry)) => {
                        pending.push_back(PurgeTask::Orphan(entry));
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }

            if pending.len() < DELETE_PURGE_ACTIVE_CAPACITY
                && claimed_retries
                    .front()
                    .is_some_and(|retry| retry.due <= Instant::now())
            {
                let retry = claimed_retries
                    .pop_front()
                    .expect("a due claimed purge retry exists");
                match self.retry_claimed_purge_work(retry.job).await {
                    Ok(Some(work)) => pending.push_back(PurgeTask::Durable(work)),
                    Ok(None) => {}
                    Err(failure) => schedule_claimed_purge_retry(
                        &mut claimed_retries,
                        failure,
                        retry.local_failures.saturating_add(1),
                    ),
                }
            }

            while pending.len() + claimed_retries.len() < DELETE_PURGE_ACTIVE_CAPACITY {
                if self.lifecycle.shutdown.is_cancelled() {
                    return;
                }
                let job = match self.state.state_store.claim_due_purge_job().await {
                    Ok(Some(job)) => job,
                    Ok(None) => break,
                    Err(error) => {
                        warn!("Failed to claim a durable purge job error={error:#}");
                        break;
                    }
                };
                match self.open_purge_work(job).await {
                    Ok(Some(work)) => pending.push_back(PurgeTask::Durable(work)),
                    Ok(None) => {}
                    Err(failure) => schedule_claimed_purge_retry(&mut claimed_retries, failure, 1),
                }
            }

            if let Some(task) = pending.pop_front() {
                match task {
                    PurgeTask::Durable(work) => match self.process_purge_work(work).await {
                        PurgeWorkResult::Complete => {}
                        PurgeWorkResult::Pending(work) => {
                            pending.push_back(PurgeTask::Durable(*work));
                        }
                        PurgeWorkResult::Retry(failure) => {
                            schedule_claimed_purge_retry(&mut claimed_retries, failure, 1);
                        }
                    },
                    PurgeTask::Orphan(entry) => match entry
                        .purge_slice(
                            DELETE_PURGE_SLICE_ENTRIES,
                            DELETE_PURGE_SLICE_TIME,
                            self.lifecycle.shutdown.clone(),
                        )
                        .await
                    {
                        Ok(TrashPurgeProgress::Complete) => {}
                        Ok(TrashPurgeProgress::Pending(entry)) => {
                            pending.push_back(PurgeTask::Orphan(entry));
                        }
                        Err(error) => {
                            warn!(
                                "Failed to purge unjournaled trash; maintenance will rediscover it error={error:#}"
                            );
                        }
                    },
                }
                tokio::task::yield_now().await;
                continue;
            }

            let retry_delay = claimed_retries
                .front()
                .map(|retry| retry.due.saturating_duration_since(Instant::now()))
                .unwrap_or(DELETE_PURGE_IDLE_POLL)
                .min(DELETE_PURGE_IDLE_POLL);
            tokio::select! {
                biased;
                _ = self.lifecycle.shutdown.cancelled() => return,
                signal = receiver.recv(), if pending.len() + claimed_retries.len() < DELETE_PURGE_ACTIVE_CAPACITY => {
                    match signal {
                        Some(PurgeSignal::Wake) => {}
                        Some(PurgeSignal::Orphan(entry)) => {
                            pending.push_back(PurgeTask::Orphan(entry));
                        }
                        None => return,
                    }
                }
                _ = tokio::time::sleep(retry_delay) => {}
            }
        }
    }

    /// Reconcile the crash gap between the checked filesystem rename and the
    /// SQLite Prepared -> Ready transition. This is a separate tracked loop so
    /// a live DELETE path lease cannot stall ordinary trash purging, and a
    /// transient full/unavailable state-store queue is retried without needing
    /// a process restart.
    pub(in crate::server) async fn run_prepared_purge_reconciler(self: Arc<Self>) {
        loop {
            if self.lifecycle.shutdown.is_cancelled() {
                return;
            }
            // Once a job has acquired its semantic path lease, do not drop its
            // future on shutdown: fd-relative quarantine may already be inside
            // a non-cancellable rename/fsync blocking closure. The reconciler
            // itself is tracked, so shutdown now observes that work and the
            // lease remains held until its filesystem and SQLite steps finish.
            if let Err(error) = self.reconcile_prepared_purge_jobs().await {
                warn!("Failed to reconcile prepared purge jobs error={error:#}");
            }
            tokio::select! {
                biased;
                _ = self.lifecycle.shutdown.cancelled() => return,
                _ = tokio::time::sleep(DELETE_PURGE_RECONCILE_INTERVAL) => {}
            }
        }
    }

    async fn retry_claimed_purge_work(
        &self,
        job: StoredPurgeJob,
    ) -> std::result::Result<Option<PurgeWork>, ClaimedPurgeFailure> {
        let current = match self.state.state_store.purge_job(job.key).await {
            Ok(Some(current)) if current.state == StoredPurgeState::Claimed => current,
            Ok(_) => return Ok(None),
            Err(source) => return Err(ClaimedPurgeFailure::new(job, source)),
        };
        self.open_purge_work(current).await
    }

    async fn open_purge_work(
        &self,
        job: StoredPurgeJob,
    ) -> std::result::Result<Option<PurgeWork>, ClaimedPurgeFailure> {
        let trash_path = match self.content.rooted_fs.resolve_state_path(&job.trash_path) {
            Ok(path) => path,
            Err(source) => return Err(ClaimedPurgeFailure::new(job, source)),
        };
        let Some(trash_revision) = job.trash_revision else {
            let error = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "durable purge job has no committed trash revision",
            );
            return match self.quarantine_claimed_purge_job(&job, &error).await {
                Ok(()) => Ok(None),
                Err(source) => Err(ClaimedPurgeFailure::new(job, source)),
            };
        };
        match self
            .content
            .rooted_fs
            .capture_entry_for_purge_with_revision(&trash_path, job.is_directory, trash_revision)
            .await
        {
            Ok(Some(entry)) if purge_identity_matches(entry.identity(), &job) => {
                Ok(Some(PurgeWork { job, entry }))
            }
            Ok(Some(entry)) => {
                drop(entry);
                let error = std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "trash entry identity changed before durable purge",
                );
                match self.quarantine_claimed_purge_job(&job, &error).await {
                    Ok(()) => Ok(None),
                    Err(source) => Err(ClaimedPurgeFailure::new(job, source)),
                }
            }
            Ok(None) => match self.state.state_store.complete_purge_job(job.key).await {
                Ok(true) => Ok(None),
                Ok(false) => {
                    warn!("A claimed purge job disappeared before completion");
                    Ok(None)
                }
                Err(source) => Err(ClaimedPurgeFailure::new(job, source)),
            },
            Err(error) => {
                let result = if error.kind() == std::io::ErrorKind::InvalidData {
                    self.quarantine_claimed_purge_job(&job, &error).await
                } else {
                    self.defer_purge_job(&job, &error).await
                };
                match result {
                    Ok(()) => Ok(None),
                    Err(source) => Err(ClaimedPurgeFailure::new(job, source)),
                }
            }
        }
    }

    async fn process_purge_work(&self, mut work: PurgeWork) -> PurgeWorkResult {
        match work
            .entry
            .purge_slice(
                DELETE_PURGE_SLICE_ENTRIES,
                DELETE_PURGE_SLICE_TIME,
                self.lifecycle.shutdown.clone(),
            )
            .await
        {
            Ok(TrashPurgeProgress::Complete) => {
                match self
                    .state
                    .state_store
                    .complete_purge_job(work.job.key)
                    .await
                {
                    Ok(_) => PurgeWorkResult::Complete,
                    Err(source) => {
                        PurgeWorkResult::Retry(ClaimedPurgeFailure::new(work.job, source))
                    }
                }
            }
            Ok(TrashPurgeProgress::Pending(entry)) => {
                work.entry = entry;
                PurgeWorkResult::Pending(Box::new(work))
            }
            Err(error) => {
                let (entry, source) = error.into_parts();
                drop(entry);
                let result = if source.kind() == std::io::ErrorKind::InvalidData {
                    self.quarantine_claimed_purge_job(&work.job, &source).await
                } else {
                    self.defer_purge_job(&work.job, &source).await
                };
                match result {
                    Ok(()) => PurgeWorkResult::Complete,
                    Err(error) => PurgeWorkResult::Retry(ClaimedPurgeFailure::new(work.job, error)),
                }
            }
        }
    }

    async fn quarantine_claimed_purge_job(
        &self,
        job: &StoredPurgeJob,
        source: &std::io::Error,
    ) -> Result<()> {
        let trash = self.content.rooted_fs.resolve_state_path(&job.trash_path)?;
        let quarantined = self
            .content
            .rooted_fs
            .quarantine_internal_trash(&trash)
            .await?;
        match quarantined {
            Some(quarantined) => warn!(
                "Quarantined an identity-ambiguous claimed trash entry and stopped automatic purge job_id={} quarantine={} error={source:#}",
                Uuid::from_bytes(job.key.id),
                quarantined.display()
            ),
            None => warn!(
                "Identity-ambiguous claimed trash entry disappeared before quarantine; completing its purge intent without touching another path job_id={} error={source:#}",
                Uuid::from_bytes(job.key.id)
            ),
        }
        if !self.state.state_store.complete_purge_job(job.key).await? {
            return Err(anyhow!(
                "claimed purge job was lost before quarantine completion"
            ));
        }
        Ok(())
    }

    async fn defer_purge_job(&self, job: &StoredPurgeJob, source: &std::io::Error) -> Result<()> {
        let failures = job.attempts.saturating_add(1);
        let delay = purge_retry_delay(failures);
        warn!(
            "Failed to purge an internal trash entry; durable retry scheduled failures={} retry_ms={} error={source:#}",
            failures,
            delay.as_millis()
        );
        if !self
            .state
            .state_store
            .retry_purge_job(job.key, delay)
            .await?
        {
            return Err(anyhow!(
                "claimed purge job was lost before retry scheduling"
            ));
        }
        Ok(())
    }

    async fn reconcile_prepared_purge_jobs(&self) -> Result<()> {
        let jobs = self
            .state
            .state_store
            .prepared_purge_jobs(DELETE_PURGE_RECONCILE_LIMIT)
            .await?;
        for job in jobs {
            if self.lifecycle.shutdown.is_cancelled() {
                break;
            }
            if let Err(error) = self.reconcile_prepared_purge_job(&job).await {
                warn!(
                    "Failed to reconcile prepared purge job job_id={} error={error:#}",
                    Uuid::from_bytes(job.key.id)
                );
            }
        }
        Ok(())
    }

    async fn reconcile_prepared_purge_job(&self, job: &StoredPurgeJob) -> Result<()> {
        let target = self
            .content
            .rooted_fs
            .resolve_state_path(&job.target_path)?;
        let trash = self.content.rooted_fs.resolve_state_path(&job.trash_path)?;
        // Live DELETE owns the same target lease from before it records the
        // Prepared intent until after rename + Ready. Waiting on that lease
        // prevents startup reconciliation from deleting an intent in the
        // narrow prepare/rename/mark-ready window.
        let _path_lease = self
            .content
            .path_coordinator
            .acquire([&target, &trash])
            .await;
        let Some(job) = self.state.state_store.purge_job(job.key).await? else {
            return Ok(());
        };
        if job.state != StoredPurgeState::Prepared {
            return Ok(());
        }
        let trash_identity = delete_identity_if_present(&self.content.rooted_fs, &trash).await?;
        if trash_identity.is_some() {
            let quarantined = self
                .content
                .rooted_fs
                .quarantine_internal_trash(&trash)
                .await?;
            if let Some(quarantined) = quarantined {
                warn!(
                    "Quarantined an identity-ambiguous internal trash entry for manual inspection job_id={} quarantine={}",
                    Uuid::from_bytes(job.key.id),
                    quarantined.display()
                );
            }
        }

        warn!(
            "Prepared purge intent has no committed trash revision; preserving the target and releasing the quarantined intent job_id={} target={} trash={}",
            Uuid::from_bytes(job.key.id),
            target.display(),
            trash.display()
        );
        // Prepared rows predate the atomic revision write. Neither the weak
        // source identity nor the current target name proves that a trash
        // occupant survived the checked rename, so any occupant is quarantined
        // and the target is never touched.
        self.state.state_store.remove_purge_job(job.key).await?;
        Ok(())
    }

    pub(in crate::server) async fn reconcile_prepared_purge_key(&self, key: PurgeJobKey) {
        let job = match self.state.state_store.purge_job(key).await {
            Ok(Some(job)) if job.state == StoredPurgeState::Prepared => job,
            Ok(_) => return,
            Err(error) => {
                warn!(
                    "Failed to reload a purge intent after an uncertain delete job_id={} error={error:#}",
                    Uuid::from_bytes(key.id)
                );
                return;
            }
        };
        if let Err(error) = self.reconcile_prepared_purge_job(&job).await {
            warn!(
                "Failed to reconcile a purge intent after an uncertain delete job_id={} error={error:#}",
                Uuid::from_bytes(key.id)
            );
        }
    }
}

fn schedule_claimed_purge_retry(
    retries: &mut VecDeque<ClaimedPurgeRetry>,
    failure: ClaimedPurgeFailure,
    local_failures: u32,
) {
    let local_failures = local_failures.max(1);
    let delay = purge_retry_delay(local_failures);
    warn!(
        "A claimed purge job could not record its next state; retaining it for an in-process retry job_id={} local_failures={} retry_ms={} error={:#}",
        Uuid::from_bytes(failure.job.key.id),
        local_failures,
        delay.as_millis(),
        failure.source
    );
    let retry = ClaimedPurgeRetry {
        due: Instant::now() + delay,
        job: *failure.job,
        local_failures,
    };
    let position = retries
        .iter()
        .position(|queued| queued.due > retry.due)
        .unwrap_or(retries.len());
    retries.insert(position, retry);
}

async fn delete_identity_if_present(
    rooted_fs: &super::rooted_fs::RootedFs,
    path: &Path,
) -> std::io::Result<Option<DeleteIdentity>> {
    match rooted_fs.delete_identity(path).await {
        Ok(identity) => Ok(Some(identity)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn purge_identity_matches(identity: DeleteIdentity, job: &StoredPurgeJob) -> bool {
    identity.device == job.source_identity.device
        && identity.inode == job.source_identity.inode
        && identity.is_directory == job.is_directory
}

pub(in crate::server) fn purge_retry_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    DELETE_PURGE_RETRY_BASE
        .saturating_mul(multiplier)
        .min(DELETE_PURGE_RETRY_MAX)
}

#[cfg(test)]
mod tests;
