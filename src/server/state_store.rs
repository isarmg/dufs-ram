use anyhow::{Context, Result, anyhow, bail, ensure};
#[cfg(test)]
use rusqlite::OpenFlags;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
#[cfg(test)]
use std::fs::{self, Permissions};
use std::{
    collections::VecDeque,
    convert::TryFrom,
    ffi::OsString,
    fmt,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;
use uuid::Uuid;

mod actor;
mod database;
mod model;
mod operation;
mod purge;
mod upload;

pub(super) use model::{
    OperationKey, PurgeJobKey, RootIdentity, StateBlockingPath, StatePathCursor, StatePathPage,
    StoreBegin, StorePurgeJob, StoreStatus, StoreUploadSession, StoredFileIdentity, StoredOutcome,
    StoredPurgeJob, StoredPurgeState, StoredTerminalState, StoredUploadSession, StoredUploadState,
    UploadSessionKey,
};

const APPLICATION_ID: i32 = 0x4455_4653; // "DUFS"
const SCHEMA_VERSION: i32 = 4;
const COMMAND_QUEUE_CAPACITY: usize = 256;
const UPLOAD_SESSION_CAPACITY: usize = 16_384;
const UPLOAD_SESSION_PER_OWNER_CAPACITY: usize = 4_096;
const PURGE_JOB_CAPACITY: usize = 4_096;
const PURGE_JOB_PER_OWNER_CAPACITY: usize = 1_024;

const OPERATION_RESERVED: i64 = 0;
const OPERATION_COMMIT_STARTED: i64 = 1;
const OPERATION_COMPLETED: i64 = 2;

const UPLOAD_RUNNING: i64 = 0;
const UPLOAD_COMMIT_STARTED: i64 = 1;
const UPLOAD_COMMITTED: i64 = 2;
const UPLOAD_REJECTED: i64 = 3;
const UPLOAD_UNKNOWN: i64 = 4;
const UPLOAD_AWAITING_CONFIRMATION: i64 = 5;

const PURGE_PREPARED: i64 = 0;
const PURGE_READY: i64 = 1;
const PURGE_CLAIMED: i64 = 2;

const UNKNOWN_STATUS: u16 = 500;
const UNKNOWN_CODE: &str = "outcome_uncertain";

const MAX_ERROR_CODE_CHARS: usize = 128;

/// A command that was rejected before the state actor accepted ownership of
/// it. Callers may safely report these failures as known, retryable
/// rejections; failures after enqueue remain ordinary errors because the
/// command's outcome may no longer be known to the requester.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StateStoreDispatchError {
    QueueFull,
    Unavailable,
}

impl fmt::Display for StateStoreDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::QueueFull => "The state store command queue is full",
            Self::Unavailable => "The state store is unavailable",
        })
    }
}

impl std::error::Error for StateStoreDispatchError {}

#[derive(Clone)]
pub(super) struct StateStore {
    inner: Arc<StateStoreInner>,
}

struct StateStoreInner {
    channels: ActorChannels,
    healthy: Arc<AtomicBool>,
    lifecycle: Mutex<ThreadLifecycle>,
    lifecycle_changed: Condvar,
    // Tests exercise the same file-backed implementation in an isolated
    // temporary directory. Keeping the directory here ties its lifetime to the
    // actor and avoids retaining a second, memory-only database mode.
    _temporary_directory: Option<tempfile::TempDir>,
}

impl Drop for StateStoreInner {
    fn drop(&mut self) {
        let _ = self.close_blocking();
    }
}

impl StateStoreInner {
    fn close_blocking(&self) -> Result<()> {
        let handle = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while lifecycle.closing && !lifecycle.closed {
                lifecycle = self
                    .lifecycle_changed
                    .wait(lifecycle)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if lifecycle.closed {
                return Ok(());
            }
            lifecycle.closing = true;
            lifecycle.handle.take()
        };

        self.healthy.store(false, Ordering::Release);
        let control_result = self.channels.send_control(ControlCommand::Shutdown);
        let join_result = handle
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| anyhow!("The state store thread panicked during shutdown"))
            })
            .unwrap_or(Ok(()));

        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.closed = true;
        lifecycle.closing = false;
        self.lifecycle_changed.notify_all();
        drop(lifecycle);

        if control_result.is_err() && join_result.is_ok() {
            // A cleanly joined worker may already have exited after reporting a
            // database failure. Closing remains idempotent; health already
            // carries the failure to callers.
            return Ok(());
        }
        join_result
    }
}

struct ThreadLifecycle {
    handle: Option<thread::JoinHandle<()>>,
    closing: bool,
    closed: bool,
}

#[derive(Clone)]
struct ActorChannels {
    commands: SyncSender<Command>,
    controls: Sender<ControlCommand>,
}

impl ActorChannels {
    fn send_control(&self, control: ControlCommand) -> Result<()> {
        self.controls
            .send(control)
            .map_err(|_| anyhow!("The state store control channel is unavailable"))?;
        match self.commands.try_send(Command::Wake) {
            Ok(()) | Err(TrySendError::Full(Command::Wake)) => Ok(()),
            Err(TrySendError::Disconnected(Command::Wake)) => {
                bail!("The state store thread is unavailable")
            }
            Err(_) => unreachable!("a wake command is returned unchanged"),
        }
    }
}

#[derive(Clone, Copy)]
struct RepositoryLimits {
    upload_capacity: usize,
    upload_per_owner: usize,
    purge_capacity: usize,
    purge_per_owner: usize,
}

const REPOSITORY_LIMITS: RepositoryLimits = RepositoryLimits {
    upload_capacity: UPLOAD_SESSION_CAPACITY,
    upload_per_owner: UPLOAD_SESSION_PER_OWNER_CAPACITY,
    purge_capacity: PURGE_JOB_CAPACITY,
    purge_per_owner: PURGE_JOB_PER_OWNER_CAPACITY,
};

struct Limits {
    capacity: i64,
    per_owner: i64,
    ttl_ms: i64,
    upload_capacity: i64,
    upload_per_owner: i64,
    purge_capacity: i64,
    purge_per_owner: i64,
}

struct StateStoreOptions {
    path: PathBuf,
    root: RootIdentity,
    capacity: usize,
    per_owner: usize,
    ttl: Duration,
    command_queue_capacity: usize,
    repository_limits: RepositoryLimits,
    temporary_directory: Option<tempfile::TempDir>,
}

impl StateStoreOptions {
    fn production(
        path: PathBuf,
        root: RootIdentity,
        capacity: usize,
        per_owner: usize,
        ttl: Duration,
    ) -> Self {
        Self {
            path,
            root,
            capacity,
            per_owner,
            ttl,
            command_queue_capacity: COMMAND_QUEUE_CAPACITY,
            repository_limits: REPOSITORY_LIMITS,
            temporary_directory: None,
        }
    }
}

enum Command {
    Wake,
    ProbeReadiness {
        reply: oneshot::Sender<Result<()>>,
    },
    Begin {
        key: OperationKey,
        fingerprint: [u8; 32],
        reply: oneshot::Sender<Result<BeginEnvelope>>,
    },
    Status {
        key: OperationKey,
        reply: oneshot::Sender<Result<StoreStatus>>,
    },
    MarkCommitStarted {
        key: OperationKey,
        lease: [u8; 16],
        reply: oneshot::Sender<Result<bool>>,
    },
    Complete {
        key: OperationKey,
        lease: [u8; 16],
        outcome: StoredOutcome,
        reply: oneshot::Sender<Result<bool>>,
    },
    SaveUploadSession {
        session: StoredUploadSession,
        ttl_ms: i64,
        reply: oneshot::Sender<Result<StoreUploadSession>>,
    },
    LoadUploadSession {
        key: UploadSessionKey,
        reply: oneshot::Sender<Result<Option<StoredUploadSession>>>,
    },
    ListExpiredUploadSessions {
        limit: i64,
        reply: oneshot::Sender<Result<Vec<StoredUploadSession>>>,
    },
    RemoveUploadSession {
        key: UploadSessionKey,
        reply: oneshot::Sender<Result<bool>>,
    },
    PreparePurgeJob {
        job: StoredPurgeJob,
        reply: oneshot::Sender<Result<StorePurgeJob>>,
    },
    LoadPurgeJob {
        key: PurgeJobKey,
        reply: oneshot::Sender<Result<Option<StoredPurgeJob>>>,
    },
    ListPreparedPurgeJobs {
        limit: i64,
        reply: oneshot::Sender<Result<Vec<StoredPurgeJob>>>,
    },
    ListPurgeJobs {
        limit: i64,
        reply: oneshot::Sender<Result<Vec<StoredPurgeJob>>>,
    },
    ListStateBlockingPaths {
        after: Option<StatePathCursor>,
        limit: i64,
        reply: oneshot::Sender<Result<StatePathPage>>,
    },
    MarkPurgeJobReady {
        key: PurgeJobKey,
        trash_revision: [u8; 32],
        reply: oneshot::Sender<Result<bool>>,
    },
    ClaimDuePurgeJob {
        reply: oneshot::Sender<Result<Option<StoredPurgeJob>>>,
    },
    RetryPurgeJob {
        key: PurgeJobKey,
        delay_ms: i64,
        reply: oneshot::Sender<Result<bool>>,
    },
    CompletePurgeJob {
        key: PurgeJobKey,
        reply: oneshot::Sender<Result<bool>>,
    },
    RemovePurgeJob {
        key: PurgeJobKey,
        reply: oneshot::Sender<Result<bool>>,
    },
    #[cfg(test)]
    InspectPragmas {
        reply: oneshot::Sender<Result<PragmaSnapshot>>,
    },
    #[cfg(test)]
    InjectSqlError {
        reply: oneshot::Sender<Result<()>>,
    },
    #[cfg(test)]
    SetQueryOnly {
        enabled: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    #[cfg(test)]
    Block {
        entered: SyncSender<()>,
        release: Receiver<()>,
    },
}

enum ControlCommand {
    Abandon { key: OperationKey, lease: [u8; 16] },
    Shutdown,
}

struct BeginEnvelope {
    begin: StoreBegin,
    cleanup: Option<ReservationCleanup>,
}

impl BeginEnvelope {
    fn new(begin: StoreBegin, channels: &ActorChannels, key: OperationKey) -> Self {
        let cleanup = match begin {
            StoreBegin::Started { lease } => Some(ReservationCleanup {
                channels: channels.clone(),
                key,
                lease,
                armed: true,
            }),
            _ => None,
        };
        Self { begin, cleanup }
    }

    fn reservation(&self) -> Option<[u8; 16]> {
        match &self.begin {
            StoreBegin::Started { lease } => Some(*lease),
            _ => None,
        }
    }

    fn disarm_cleanup(&mut self) {
        if let Some(cleanup) = self.cleanup.as_mut() {
            cleanup.armed = false;
        }
    }

    fn disarm(mut self) -> StoreBegin {
        self.disarm_cleanup();
        self.begin
    }
}

struct ReservationCleanup {
    channels: ActorChannels,
    key: OperationKey,
    lease: [u8; 16],
    armed: bool,
}

impl Drop for ReservationCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.channels.send_control(ControlCommand::Abandon {
                key: self.key,
                lease: self.lease,
            });
        }
    }
}

impl StateStore {
    pub(super) fn open(
        path: &Path,
        root: &RootIdentity,
        capacity: usize,
        per_owner: usize,
        ttl: Duration,
    ) -> Result<Self> {
        database::prepare_database_file(path, root)?;
        Self::start(StateStoreOptions::production(
            path.to_path_buf(),
            *root,
            capacity,
            per_owner,
            ttl,
        ))
    }

    #[cfg(test)]
    pub(super) fn temporary_for_test(
        capacity: usize,
        per_owner: usize,
        ttl: Duration,
    ) -> Result<Self> {
        Self::temporary_with_limits_for_test(
            capacity,
            per_owner,
            ttl,
            COMMAND_QUEUE_CAPACITY,
            REPOSITORY_LIMITS,
        )
    }

    fn start(options: StateStoreOptions) -> Result<Self> {
        let StateStoreOptions {
            path,
            root,
            capacity,
            per_owner,
            ttl,
            command_queue_capacity,
            repository_limits,
            temporary_directory,
        } = options;
        ensure!(
            command_queue_capacity > 0,
            "State store command queue capacity must be positive"
        );
        let limits = validate_limits(capacity, per_owner, ttl, repository_limits)?;
        let (command_sender, command_receiver) = mpsc::sync_channel(command_queue_capacity);
        let (control_sender, control_receiver) = mpsc::channel();
        let channels = ActorChannels {
            commands: command_sender,
            controls: control_sender,
        };
        let thread_channels = channels.clone();
        let healthy = Arc::new(AtomicBool::new(false));
        let thread_healthy = healthy.clone();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);

        let handle = thread::Builder::new()
            .name("dufs-state-store".to_string())
            .spawn(move || {
                actor::run(actor::ActorRuntime {
                    path,
                    root,
                    limits,
                    command_receiver,
                    control_receiver,
                    channels: thread_channels,
                    healthy: thread_healthy,
                    ready: ready_sender,
                });
            })
            .context("Failed to start the state store thread")?;

        let ready = ready_receiver
            .recv()
            .context("The state store thread exited during startup")?;
        if let Err(error) = ready {
            let _ = handle.join();
            return Err(error);
        }
        Ok(Self {
            inner: Arc::new(StateStoreInner {
                channels,
                healthy,
                lifecycle: Mutex::new(ThreadLifecycle {
                    handle: Some(handle),
                    closing: false,
                    closed: false,
                }),
                lifecycle_changed: Condvar::new(),
                _temporary_directory: temporary_directory,
            }),
        })
    }

    #[cfg(test)]
    fn temporary_with_limits_for_test(
        capacity: usize,
        per_owner: usize,
        ttl: Duration,
        command_queue_capacity: usize,
        repository_limits: RepositoryLimits,
    ) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory =
            tempfile::tempdir().context("Failed to create temporary state directory")?;
        fs::set_permissions(directory.path(), Permissions::from_mode(0o700))?;
        let path = directory.path().join("state.sqlite3");
        let root = RootIdentity {
            device: 0,
            inode: 0,
        };
        database::prepare_database_file(&path, &root)?;
        Self::start(StateStoreOptions {
            path,
            root,
            capacity,
            per_owner,
            ttl,
            command_queue_capacity,
            repository_limits,
            temporary_directory: Some(directory),
        })
    }

    pub(super) async fn begin_operation(
        &self,
        key: OperationKey,
        fingerprint: [u8; 32],
    ) -> Result<StoreBegin> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::Begin {
            key,
            fingerprint,
            reply,
        })?;
        let envelope = self.receive(receiver).await?;
        // There is intentionally no await point between receiving the envelope
        // and disarming its cleanup. Cancellation before delivery therefore
        // abandons the durable reservation, while successful delivery transfers
        // responsibility for the lease to OperationGuard.
        Ok(envelope.disarm())
    }

    pub(super) async fn operation_status(&self, key: OperationKey) -> Result<StoreStatus> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::Status { key, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn mark_operation_commit_started(
        &self,
        key: OperationKey,
        lease: [u8; 16],
    ) -> Result<bool> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::MarkCommitStarted { key, lease, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn complete_operation(
        &self,
        key: OperationKey,
        lease: [u8; 16],
        outcome: StoredOutcome,
    ) -> Result<bool> {
        outcome.validate()?;
        let (reply, receiver) = oneshot::channel();
        self.send(Command::Complete {
            key,
            lease,
            outcome,
            reply,
        })?;
        self.receive(receiver).await
    }

    pub(super) fn abandon_operation(&self, key: OperationKey, lease: [u8; 16]) {
        if self
            .inner
            .channels
            .send_control(ControlCommand::Abandon { key, lease })
            .is_err()
        {
            self.inner.healthy.store(false, Ordering::Release);
        }
    }

    pub(super) async fn save_upload_session(
        &self,
        session: StoredUploadSession,
        ttl: Duration,
    ) -> Result<StoreUploadSession> {
        session.validate()?;
        let ttl_ms = duration_ms(ttl, "Upload session TTL")?;
        let (reply, receiver) = oneshot::channel();
        self.send(Command::SaveUploadSession {
            session,
            ttl_ms,
            reply,
        })?;
        self.receive(receiver).await
    }

    pub(super) async fn upload_session(
        &self,
        key: UploadSessionKey,
    ) -> Result<Option<StoredUploadSession>> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::LoadUploadSession { key, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn expired_upload_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<StoredUploadSession>> {
        let limit = query_limit(limit)?;
        let (reply, receiver) = oneshot::channel();
        self.send(Command::ListExpiredUploadSessions { limit, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn remove_upload_session(&self, key: UploadSessionKey) -> Result<bool> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::RemoveUploadSession { key, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn prepare_purge_job(&self, job: StoredPurgeJob) -> Result<StorePurgeJob> {
        job.validate_new()?;
        let (reply, receiver) = oneshot::channel();
        self.send(Command::PreparePurgeJob { job, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn purge_job(&self, key: PurgeJobKey) -> Result<Option<StoredPurgeJob>> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::LoadPurgeJob { key, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn prepared_purge_jobs(&self, limit: usize) -> Result<Vec<StoredPurgeJob>> {
        let limit = query_limit(limit)?;
        let (reply, receiver) = oneshot::channel();
        self.send(Command::ListPreparedPurgeJobs { limit, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn purge_jobs(&self, limit: usize) -> Result<Vec<StoredPurgeJob>> {
        let limit = query_limit(limit)?;
        let (reply, receiver) = oneshot::channel();
        self.send(Command::ListPurgeJobs { limit, reply })?;
        self.receive(receiver).await
    }

    /// Return a bounded, keyset-paginated snapshot of paths whose physical
    /// meaning must not be changed by a namespace mutation. The caller must
    /// hold every affected mutation lease across all pages; those leases
    /// prevent a new relevant upload/purge row from appearing behind the
    /// cursor.
    pub(super) async fn state_blocking_paths(
        &self,
        after: Option<StatePathCursor>,
        limit: usize,
    ) -> Result<StatePathPage> {
        let limit = query_limit(limit)?;
        let (reply, receiver) = oneshot::channel();
        self.send(Command::ListStateBlockingPaths {
            after,
            limit,
            reply,
        })?;
        self.receive(receiver).await
    }

    pub(super) async fn mark_purge_job_ready(
        &self,
        key: PurgeJobKey,
        trash_revision: [u8; 32],
    ) -> Result<bool> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::MarkPurgeJobReady {
            key,
            trash_revision,
            reply,
        })?;
        self.receive(receiver).await
    }

    pub(super) async fn claim_due_purge_job(&self) -> Result<Option<StoredPurgeJob>> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::ClaimDuePurgeJob { reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn retry_purge_job(&self, key: PurgeJobKey, delay: Duration) -> Result<bool> {
        let delay_ms = duration_ms(delay, "Purge retry delay")?;
        let (reply, receiver) = oneshot::channel();
        self.send(Command::RetryPurgeJob {
            key,
            delay_ms,
            reply,
        })?;
        self.receive(receiver).await
    }

    pub(super) async fn complete_purge_job(&self, key: PurgeJobKey) -> Result<bool> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::CompletePurgeJob { key, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn remove_purge_job(&self, key: PurgeJobKey) -> Result<bool> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::RemovePurgeJob { key, reply })?;
        self.receive(receiver).await
    }

    pub(super) async fn close(&self) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.inner.close_blocking())
            .await
            .map_err(|error| anyhow!("Failed to join state store shutdown task: {error}"))?
    }

    pub(super) fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::Acquire)
    }

    /// Exercise the live actor connection with both a read and a rolled-back
    /// write transaction. Unlike `is_healthy`, this detects storage failures
    /// that occurred after the actor initialized.
    pub(super) async fn probe_readiness(&self) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::ProbeReadiness { reply })?;
        self.receive(receiver).await
    }

    fn send(&self, command: Command) -> std::result::Result<(), StateStoreDispatchError> {
        if !self.is_healthy() {
            return Err(StateStoreDispatchError::Unavailable);
        }
        match self.inner.channels.commands.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StateStoreDispatchError::QueueFull),
            Err(TrySendError::Disconnected(_)) => {
                self.inner.healthy.store(false, Ordering::Release);
                return Err(StateStoreDispatchError::Unavailable);
            }
        }
        Ok(())
    }

    async fn receive<T>(&self, receiver: oneshot::Receiver<Result<T>>) -> Result<T> {
        match receiver.await {
            Ok(result) => result,
            Err(_) => {
                self.inner.healthy.store(false, Ordering::Release);
                bail!("The state store thread exited before replying")
            }
        }
    }

    #[cfg(test)]
    async fn inspect_pragmas(&self) -> Result<PragmaSnapshot> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::InspectPragmas { reply })?;
        self.receive(receiver).await
    }

    #[cfg(test)]
    async fn inject_sql_error(&self) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::InjectSqlError { reply })?;
        self.receive(receiver).await
    }

    #[cfg(test)]
    async fn set_query_only(&self, enabled: bool) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::SetQueryOnly { enabled, reply })?;
        self.receive(receiver).await
    }

    #[cfg(test)]
    pub(super) fn shutdown_for_test(self) {
        let _ = self.inner.close_blocking();
    }

    #[cfg(test)]
    pub(super) fn block_actor_for_test(&self) -> Result<SyncSender<()>> {
        let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        self.send(Command::Block {
            entered: entered_sender,
            release: release_receiver,
        })?;
        entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .context("State store did not enter the test block")?;
        Ok(release_sender)
    }

    #[cfg(test)]
    pub(super) fn saturate_command_queue_for_test(&self) -> Result<SyncSender<()>> {
        let release_sender = self.block_actor_for_test()?;
        loop {
            match self.inner.channels.commands.try_send(Command::Wake) {
                Ok(()) => {}
                Err(TrySendError::Full(Command::Wake)) => return Ok(release_sender),
                Err(TrySendError::Disconnected(Command::Wake)) => {
                    bail!("State store disconnected while saturating its test queue")
                }
                Err(_) => unreachable!("a wake command is returned unchanged"),
            }
        }
    }
}

fn validate_limits(
    capacity: usize,
    per_owner: usize,
    ttl: Duration,
    repository: RepositoryLimits,
) -> Result<Limits> {
    ensure!(capacity > 0, "State store capacity must be positive");
    ensure!(
        (1..=capacity).contains(&per_owner),
        "Per-owner state store capacity must be positive and no greater than global capacity"
    );
    let capacity = i64::try_from(capacity).context("State store capacity is too large")?;
    let per_owner =
        i64::try_from(per_owner).context("Per-owner state store capacity is too large")?;
    let ttl_ms = i64::try_from(ttl.as_millis()).context("State store result TTL is too large")?;
    ensure!(
        repository.upload_capacity > 0
            && (1..=repository.upload_capacity).contains(&repository.upload_per_owner),
        "Upload session capacities are invalid"
    );
    ensure!(
        repository.purge_capacity > 0
            && (1..=repository.purge_capacity).contains(&repository.purge_per_owner),
        "Purge job capacities are invalid"
    );
    Ok(Limits {
        capacity,
        per_owner,
        ttl_ms,
        upload_capacity: i64::try_from(repository.upload_capacity)
            .context("Upload session capacity is too large")?,
        upload_per_owner: i64::try_from(repository.upload_per_owner)
            .context("Per-owner upload session capacity is too large")?,
        purge_capacity: i64::try_from(repository.purge_capacity)
            .context("Purge job capacity is too large")?,
        purge_per_owner: i64::try_from(repository.purge_per_owner)
            .context("Per-owner purge job capacity is too large")?,
    })
}

fn duration_ms(duration: Duration, description: &str) -> Result<i64> {
    i64::try_from(duration.as_millis()).with_context(|| format!("{description} is too large"))
}

fn query_limit(limit: usize) -> Result<i64> {
    ensure!(limit > 0, "State store query limit must be positive");
    i64::try_from(limit).context("State store query limit is too large")
}

struct StoreWorker {
    connection: Connection,
    limits: Limits,
    deferred_abandons: VecDeque<(OperationKey, [u8; 16])>,
}

#[cfg(test)]
impl StoreWorker {
    fn inspect_pragmas(&self) -> Result<PragmaSnapshot> {
        Ok(PragmaSnapshot {
            journal_mode: self
                .connection
                .pragma_query_value(None, "journal_mode", |row| row.get(0))?,
            synchronous: self
                .connection
                .pragma_query_value(None, "synchronous", |row| row.get(0))?,
            foreign_keys: self
                .connection
                .pragma_query_value(None, "foreign_keys", |row| row.get(0))?,
            trusted_schema: self
                .connection
                .pragma_query_value(None, "trusted_schema", |row| row.get(0))?,
            mmap_size: self
                .connection
                .pragma_query_value(None, "mmap_size", |row| row.get(0))?,
            application_id: self
                .connection
                .pragma_query_value(None, "application_id", |row| row.get(0))?,
            user_version: self
                .connection
                .pragma_query_value(None, "user_version", |row| row.get(0))?,
        })
    }
}

fn now_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("The system clock is before the Unix epoch")?;
    i64::try_from(elapsed.as_millis())
        .context("The system clock cannot be represented in milliseconds")
}

fn expiration_time(now: i64, ttl_ms: i64) -> Result<i64> {
    now.checked_add(ttl_ms)
        .ok_or_else(|| anyhow!("Operation expiration time overflowed"))
}

#[cfg(test)]
#[derive(Debug)]
struct PragmaSnapshot {
    journal_mode: String,
    synchronous: i64,
    foreign_keys: i64,
    trusted_schema: i64,
    mmap_size: i64,
    application_id: i64,
    user_version: i64,
}

#[cfg(test)]
mod tests;
