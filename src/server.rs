mod assets;
mod blocking_io;
mod browser_api;
mod delete;
mod disk_space;
mod download;
mod identity;
mod internal_names;
mod listing;
mod login_rate_limit;
mod maintenance;
mod operation_registry;
mod path_coordinator;
mod path_policy;
mod problem;
mod protocol;
mod purge;
mod rooted_fs;
mod router;
mod session;
mod state_store;
mod storage;
mod upload;

#[cfg(test)]
use self::purge::{DELETE_PURGE_RETRY_BASE, DELETE_PURGE_RETRY_MAX, purge_retry_delay};
use self::{
    assets::embedded_assets_prefix,
    disk_space::DiskSpaceTracker,
    listing::ListSnapshotCache,
    login_rate_limit::LoginRateLimiter,
    maintenance::UPLOAD_SESSION_TTL,
    operation_registry::OperationRegistry,
    path_coordinator::PathCoordinator,
    path_policy::{PathPolicy, RootedPath, RoutePath},
    problem::{ErrorCode, RecoveryAdvice},
    protocol::UploadPublicState,
    purge::{PurgeQueue, PurgeSignal},
    rooted_fs::{RootedEntryKey, RootedFs},
    session::{LoginBodyAdmission, LoginErrorStore},
    state_store::StateStore,
    storage::DurableStorage,
    upload::{UploadOptions, UploadRecordStore},
};
use crate::{
    Args, app_error::AppError, args::ValidatedConfig, auth::AccessControl, http_utils::body_full,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use headers::{ContentType, HeaderMapExt};
use http_body_util::combinators::BoxBody;
use hyper::{
    Method, StatusCode,
    body::Incoming,
    header::{ALLOW, CACHE_CONTROL, CONTENT_DISPOSITION, HeaderValue},
};
use std::{
    collections::HashSet,
    future::Future,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};
use tokio::sync::{OwnedRwLockReadGuard, RwLock, Semaphore, mpsc};
use tokio::time::timeout_at;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub type Request = hyper::Request<Incoming>;
pub type Response = hyper::Response<BoxBody<Bytes, anyhow::Error>>;

const BUF_SIZE: usize = 65536;
const HEALTH_CHECK_PATH: &str = "__dufs__/health";
const READINESS_CHECK_PATH: &str = "__dufs__/ready";
const AUTH_ERROR_HEADER: &str = "x-dufs-auth-error";
const CSRF_AUTH_ERROR: &str = "csrf";
const NON_UPLOAD_MUTATION_CAPACITY: usize = 64;
const STATE_PATH_ADMISSION_PAGE_SIZE: usize = 256;

/// Builds a reusable server together with the lifecycle resources required by
/// its background maintenance work.
pub struct ServerBuilder {
    args: Args,
    isolated_list_snapshot_cache: bool,
}

impl ServerBuilder {
    pub fn new(args: Args) -> Self {
        Self {
            args,
            isolated_list_snapshot_cache: false,
        }
    }

    /// Gives this server its own bounded directory-listing snapshot cache.
    ///
    /// By default, servers in the same process share one cache so that
    /// cursors remain usable across interchangeable server instances. Enable
    /// this option for tenant isolation or deterministic cache lifecycles.
    pub fn with_isolated_list_snapshot_cache(mut self) -> Self {
        self.isolated_list_snapshot_cache = true;
        self
    }

    pub fn build(self) -> Result<ServerRuntime> {
        // TaskTracker::spawn requires an entered Tokio runtime. Return a
        // normal construction error instead of letting an embedder encounter
        // a runtime panic in `start_maintenance`.
        let _runtime = tokio::runtime::Handle::try_current()
            .context("ServerBuilder::build requires an active Tokio runtime")?;
        let lifecycle = ServerLifecycle::new();
        let server = if self.isolated_list_snapshot_cache {
            Server::init_with_list_snapshot_cache(
                self.args,
                lifecycle.clone(),
                ListSnapshotCache::isolated(),
            )?
        } else {
            Server::init_with_lifecycle(self.args, lifecycle.clone())?
        };
        let server = Arc::new(server);
        server.start_maintenance();
        Ok(ServerRuntime { server, lifecycle })
    }
}

/// Owns a reusable [`Server`] and coordinates its background-task shutdown.
///
/// Construct this runtime through [`ServerBuilder`]. Call [`Self::shutdown`]
/// to wait for tracked work to drain; dropping the runtime performs a
/// non-blocking forced teardown as a safety net.
pub struct ServerRuntime {
    server: Arc<Server>,
    lifecycle: ServerLifecycle,
}

impl ServerRuntime {
    pub fn server(&self) -> &Arc<Server> {
        &self.server
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.lifecycle.shutdown.clone()
    }

    pub fn force_shutdown_token(&self) -> CancellationToken {
        self.lifecycle.force_shutdown.clone()
    }

    pub fn request_force_shutdown(&self) {
        self.lifecycle.running.store(false, Ordering::SeqCst);
        self.lifecycle.shutdown.cancel();
        self.lifecycle.force_shutdown.cancel();
    }

    /// Returns the number of ordinary/background tasks and durable filesystem
    /// mutations that are still tracked by this runtime.
    pub fn active_task_counts(&self) -> (usize, usize) {
        (
            self.lifecycle.work_tasks.len(),
            self.lifecycle.commit_tasks.len(),
        )
    }

    /// Stops maintenance and waits until tracked work, durable filesystem
    /// mutations, and the state store have shut down cleanly.
    ///
    /// The method borrows the runtime so a process supervisor can observe the
    /// task counts or request forced cancellation while awaiting the drain.
    pub async fn shutdown(&self) {
        self.lifecycle.shutdown.cancel();
        // The writer is an atomic admission barrier: it cannot be acquired
        // until every request that passed the entry check has returned, and a
        // queued writer prevents later readers from slipping in. Consequently
        // no request can register a detached task after the drains below.
        let _request_drain = self.lifecycle.request_gate.write().await;
        self.lifecycle.work_tasks.close();
        self.lifecycle.work_tasks.wait().await;
        self.lifecycle.commit_tasks.close();
        self.lifecycle.commit_tasks.wait().await;
        if let Err(error) = self.server.state.state_store.close().await {
            error!("Failed to close state store cleanly error={error:#}");
        }
        self.lifecycle.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for ServerRuntime {
    fn drop(&mut self) {
        // Drop cannot wait for asynchronous work, but it must break the
        // maintenance tasks' Arc<Server> ownership loop if an embedder forgets
        // to await `shutdown`.
        self.request_force_shutdown();
    }
}

pub struct Server {
    content: ContentServices,
    state: DurableStateServices,
    admission: AdmissionControl,
    lifecycle: ServerLifecycle,
}

/// Request-time services concerned with authentication, names, and rooted
/// filesystem access. These values share a configuration lifetime but do not
/// own background-task shutdown or durable command processing.
struct ContentServices {
    args: ValidatedConfig,
    auth: AccessControl,
    assets_prefix: String,
    path_policy: PathPolicy,
    path_coordinator: PathCoordinator,
    rooted_fs: RootedFs,
    storage: DurableStorage,
    list_snapshot_cache: ListSnapshotCache,
}

/// Durable control-plane state and its maintenance queue.
struct DurableStateServices {
    operation_registry: OperationRegistry,
    state_store: StateStore,
    upload_records: UploadRecordStore,
    purge_queue: PurgeQueue,
    purge_receiver: Mutex<Option<mpsc::Receiver<PurgeSignal>>>,
}

/// Bounded admission and accounting resources. Grouping these makes capacity
/// policy independently reviewable from routing and persistence.
struct AdmissionControl {
    active_upload_files: Arc<Mutex<HashSet<RootedEntryKey>>>,
    login_slots: Arc<Semaphore>,
    login_body_admission: LoginBodyAdmission,
    login_rate_limiter: LoginRateLimiter,
    upload_slots: Arc<Semaphore>,
    mutation_slots: Arc<Semaphore>,
    search_slots: Arc<Semaphore>,
    disk_space: DiskSpaceTracker,
    login_errors: Mutex<LoginErrorStore>,
}

/// Process lifecycle and task ownership. Only this context decides when new
/// work stops and when detached mutations have drained.
#[derive(Clone)]
struct ServerLifecycle {
    running: Arc<AtomicBool>,
    work_tasks: TaskTracker,
    commit_tasks: TaskTracker,
    shutdown: CancellationToken,
    force_shutdown: CancellationToken,
    request_gate: Arc<RwLock<()>>,
}

impl ServerLifecycle {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            work_tasks: TaskTracker::new(),
            commit_tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            force_shutdown: CancellationToken::new(),
            request_gate: Arc::new(RwLock::new(())),
        }
    }

    async fn enter_request(&self) -> Option<OwnedRwLockReadGuard<()>> {
        let guard = self.request_gate.clone().read_owned().await;
        (!self.shutdown.is_cancelled()).then_some(guard)
    }
}

impl Server {
    pub fn builder(args: Args) -> ServerBuilder {
        ServerBuilder::new(args)
    }

    fn init_with_lifecycle(args: Args, lifecycle: ServerLifecycle) -> Result<Self> {
        Self::init_with_list_snapshot_cache(args, lifecycle, ListSnapshotCache::shared_process())
    }

    fn init_with_list_snapshot_cache(
        args: Args,
        lifecycle: ServerLifecycle,
        list_snapshot_cache: ListSnapshotCache,
    ) -> Result<Self> {
        let args = ValidatedConfig::try_from(args)?;
        let auth = AccessControl::from_config(args.auth.clone());
        let assets_prefix = embedded_assets_prefix();
        let rooted_fs = RootedFs::new(&args.serve_path)?;
        let path_policy = PathPolicy::new(args.serve_path.clone(), &assets_prefix);
        let max_concurrent_uploads = args.max_concurrent_uploads;
        let max_concurrent_searches = args.max_concurrent_searches;
        let (purge_queue, purge_receiver) = PurgeQueue::new();
        let storage = DurableStorage::new(rooted_fs.clone());
        let state_database_path = args.state_database_path();
        let (device, inode) = rooted_fs.root_identity();
        let operation_registry = OperationRegistry::open(
            &state_database_path,
            state_store::RootIdentity { device, inode },
        )?;
        let state_store = operation_registry.state_store();
        let upload_records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), UPLOAD_SESSION_TTL)?;
        upload_records.reconcile_stage_layouts()?;
        Ok(Self {
            content: ContentServices {
                args,
                auth,
                assets_prefix,
                path_policy,
                path_coordinator: PathCoordinator::new(rooted_fs.clone()),
                rooted_fs,
                storage,
                list_snapshot_cache,
            },
            state: DurableStateServices {
                operation_registry,
                state_store,
                upload_records,
                purge_queue,
                purge_receiver: Mutex::new(Some(purge_receiver)),
            },
            admission: AdmissionControl {
                active_upload_files: Arc::new(Mutex::new(HashSet::new())),
                login_slots: Arc::new(Semaphore::new(2)),
                login_body_admission: LoginBodyAdmission::default(),
                login_rate_limiter: LoginRateLimiter::new(),
                upload_slots: Arc::new(Semaphore::new(max_concurrent_uploads)),
                mutation_slots: Arc::new(Semaphore::new(NON_UPLOAD_MUTATION_CAPACITY)),
                search_slots: Arc::new(Semaphore::new(max_concurrent_searches)),
                disk_space: DiskSpaceTracker::new(),
                login_errors: Mutex::new(LoginErrorStore::default()),
            },
            lifecycle,
        })
    }

    pub(crate) fn start_maintenance(self: &Arc<Self>) {
        let receiver = self
            .state
            .purge_receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(receiver) = receiver else {
            return;
        };
        let server = self.clone();
        drop(self.lifecycle.work_tasks.spawn(async move {
            server.run_purge_worker(receiver).await;
        }));
        let server = self.clone();
        drop(self.lifecycle.work_tasks.spawn(async move {
            server.run_prepared_purge_reconciler().await;
        }));
        let server = self.clone();
        drop(self.lifecycle.work_tasks.spawn(async move {
            server.run_storage_maintenance().await;
        }));
    }

    #[cfg(test)]
    pub(super) async fn run_commit<F, T>(&self, task: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        self.run_commit_inner(None, task).await
    }

    pub(in crate::server) async fn run_operation_commit<F, T>(
        &self,
        mutation: router::MutationProgress,
        task: F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        self.run_commit_inner(Some(mutation), task).await
    }

    async fn run_commit_inner<F, T>(
        &self,
        mutation: Option<router::MutationProgress>,
        task: F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let permit = self
            .admission
            .mutation_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("filesystem mutation admission was closed"))?;
        if let Some(mutation) = mutation {
            mutation.mark_detached_commit();
        }
        self.lifecycle
            .commit_tasks
            .spawn(async move {
                let _permit = permit;
                let result = task.await;
                if let Err(err) = &result {
                    error!("Tracked filesystem mutation failed error={err:#}");
                }
                result
            })
            .await?
    }

    /// Inspect every durable filesystem obligation in bounded keyset pages.
    /// Callers must hold mutation leases for all `paths` until they either
    /// reject the request or finish the filesystem commit. That invariant
    /// prevents a newly imported upload session or purge intent from appearing
    /// behind the pagination cursor.
    async fn has_persisted_path_conflict(&self, paths: &[&Path]) -> Result<bool> {
        let mut after = None;
        loop {
            let page = self
                .state
                .state_store
                .state_blocking_paths(after, STATE_PATH_ADMISSION_PAGE_SIZE)
                .await?;
            for path in paths {
                if self
                    .content
                    .path_coordinator
                    .conflicts_with_state_paths(path, &page.paths)
                    .await
                {
                    return Ok(true);
                }
            }
            let Some(next) = page.next else {
                return Ok(false);
            };
            after = Some(next);
        }
    }

    /// A fresh PUT only replaces its leaf entry, so an equal persisted upload
    /// target remains valid. It must still reject replacing a directory or
    /// symlink that gives any durable upload/purge path its current meaning.
    async fn has_persisted_path_descendant(&self, path: &Path) -> Result<bool> {
        let mut after = None;
        loop {
            let page = self
                .state
                .state_store
                .state_blocking_paths(after, STATE_PATH_ADMISSION_PAGE_SIZE)
                .await?;
            if self
                .content
                .path_coordinator
                .has_state_path_descendant(path, &page.paths)
                .await
            {
                return Ok(true);
            }
            let Some(next) = page.next else {
                return Ok(false);
            };
            after = Some(next);
        }
    }

    async fn run_tracked_upload(
        self: &Arc<Self>,
        path: &Path,
        options: UploadOptions,
        req: Request,
        upload_permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<Response> {
        let server = self.clone();
        let path = path.to_path_buf();
        let deadline = options.deadline;
        let upload_id = options.upload_id;
        let upload_length = options.upload_length;
        let upload_offset = options.mode.offset();
        let mutation = options.mutation.clone();
        let mut task = self.lifecycle.commit_tasks.spawn(async move {
            let _upload_permit = upload_permit;
            let mut response = Response::default();
            let result = server
                .handle_upload(&path, options, req, &mut response)
                .await;
            if let Err(error) = &result {
                error!("Tracked upload failed error={error:#}");
            }
            result.map(|()| response)
        });
        match timeout_at(deadline, &mut task).await {
            Ok(result) => result?,
            Err(_) => {
                if mutation.cancel_upload_before_mutation() {
                    task.abort();
                    let mut response = Response::default();
                    upload::apply_upload_problem(
                        &mut response,
                        upload::UploadErrorContext::new(
                            upload_id,
                            UploadPublicState::NotStarted,
                            Some(upload_length),
                            upload_offset,
                        ),
                        StatusCode::REQUEST_TIMEOUT,
                        ErrorCode::REQUEST_TIMEOUT,
                        "Upload deadline exceeded before any upload mutation",
                        RecoveryAdvice::Retry,
                    )?;
                    return Ok(response);
                }
                let mut response = Response::default();
                upload::apply_upload_problem(
                    &mut response,
                    upload::UploadErrorContext::new(
                        upload_id,
                        UploadPublicState::Unknown,
                        Some(upload_length),
                        upload_offset,
                    ),
                    StatusCode::REQUEST_TIMEOUT,
                    ErrorCode::UPLOAD_OUTCOME_UNKNOWN,
                    "Upload deadline exceeded; the final result is unknown",
                    RecoveryAdvice::QueryUpload,
                )?;
                Ok(response)
            }
        }
    }

    fn send_liveness(&self, head_only: bool, res: &mut Response) {
        const BODY: &str = r#"{"status":"OK"}"#;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if !head_only {
            *res.body_mut() = body_full(BODY);
        }
    }

    async fn send_readiness(&self, head_only: bool, res: &mut Response) {
        const READY: &str = r#"{"status":"ready"}"#;
        const NOT_READY: &str = r#"{"status":"not_ready"}"#;
        let (root_probe, disk_probe, state_probe) = tokio::join!(
            self.content.rooted_fs.probe_writable(),
            self.admission.disk_space.reserve_async(
                self.content.rooted_fs.root_handle(),
                0,
                self.content.args.min_free_space,
            ),
            self.state.state_store.probe_readiness(),
        );
        let ready = root_probe.is_ok()
            && disk_probe.is_ok_and(|reservation| reservation.is_some())
            && state_probe.is_ok()
            && self.state.operation_registry.is_healthy()
            && !self.lifecycle.shutdown.is_cancelled()
            && !self.lifecycle.force_shutdown.is_cancelled();
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        *res.status_mut() = if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        if !head_only {
            *res.body_mut() = body_full(if ready { READY } else { NOT_READY });
        }
    }

    /// Resolve the target for method dispatch without following an invalid
    /// final symlink or exposing a link that leaves the rooted namespace.
    async fn route_metadata(&self, path: &Path) -> std::io::Result<Option<std::fs::Metadata>> {
        match self.content.rooted_fs.metadata(path).await {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                match self.content.rooted_fs.metadata_nofollow(path).await {
                    Ok(metadata) => Ok(Some(metadata)),
                    Err(fallback)
                        if matches!(
                            fallback.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        Ok(None)
                    }
                    Err(fallback) => Err(fallback),
                }
            }
            // openat2 reports XDEV when an absolute symlink or a relative
            // symlink would leave the rooted namespace. Never fall back to
            // no-follow metadata in that case: the whole path stays invisible.
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
                Ok(None)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn guard_root_contained(&self, path: &Path) -> std::io::Result<bool> {
        let mut check_path = path.to_path_buf();
        loop {
            match self.content.rooted_fs.metadata(&check_path).await {
                Ok(_) => return Ok(false),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    match check_path.parent() {
                        Some(parent) if check_path != self.content.args.serve_path => {
                            check_path = parent.to_path_buf();
                        }
                        _ => return Ok(true),
                    }
                }
                // openat2 reports XDEV when a symlink would leave the rooted
                // filesystem. That case is intentionally hidden as 404.
                Err(error)
                    if error.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) =>
                {
                    return Ok(true);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn resolve_path(&self, raw_path: &str) -> Option<RoutePath> {
        self.content.path_policy.parse_route(raw_path)
    }

    fn join_path(&self, path: &RoutePath) -> RootedPath {
        self.content.path_policy.resolve_route(path)
    }

    pub(super) fn is_managed_root(&self, path: &Path) -> bool {
        self.content.path_policy.is_managed_root(path)
    }

    pub(super) fn is_reserved_internal_component(&self, component: &str) -> bool {
        self.content.path_policy.is_reserved_component(component)
    }
}

fn to_timestamp(time: &SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn status_forbid(res: &mut Response) {
    *res.status_mut() = StatusCode::FORBIDDEN;
    *res.body_mut() = body_full("Forbidden");
}

fn status_csrf_forbid(res: &mut Response) {
    status_forbid(res);
    res.headers_mut()
        .insert(AUTH_ERROR_HEADER, HeaderValue::from_static(CSRF_AUTH_ERROR));
}

fn status_not_found(res: &mut Response) {
    *res.status_mut() = StatusCode::NOT_FOUND;
    *res.body_mut() = body_full("Not Found");
}

fn status_no_content(res: &mut Response) {
    *res.status_mut() = StatusCode::NO_CONTENT;
}

fn status_bad_request(res: &mut Response, body: &str) {
    status_error(res, StatusCode::BAD_REQUEST, body);
}

fn status_method_not_allowed(res: &mut Response, allow: &'static str) {
    *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    res.headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
}

fn status_error(res: &mut Response, status: StatusCode, body: &str) {
    apply_app_error(res, &AppError::public(status, body));
}

fn apply_app_error(res: &mut Response, error: &AppError) {
    *res.status_mut() = error.status();
    if !error.public_message().is_empty() {
        *res.body_mut() = body_full(error.public_message().to_string());
    }
}

fn set_private_no_store(res: &mut Response) {
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
}

fn set_content_disposition(res: &mut Response, filename: &str) -> Result<()> {
    let filename: String = filename
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let value = HeaderValue::from_str(&format!(
        "attachment; filename=\"download\"; filename*=UTF-8''{}",
        encode_content_disposition_filename(&filename),
    ))?;
    res.headers_mut().insert(CONTENT_DISPOSITION, value);
    Ok(())
}

fn encode_content_disposition_filename(filename: &str) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(filename.len());
    for byte in filename.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn head_only_for(method: &Method) -> bool {
    *method == Method::HEAD
}

#[cfg(test)]
mod tests;
