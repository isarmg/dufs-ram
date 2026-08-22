use super::{
    Response, Server,
    internal_names::is_internal_name,
    problem::{ApiError, ErrorCode, RecoveryAdvice, render_problem},
    rooted_fs::{RootedDirEntry, RootedFs},
};
use crate::{auth::SessionInfo, http_utils::body_full};

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use headers::{CacheControl, ContentLength, ContentType, HeaderMapExt};
use hyper::{StatusCode, header::HeaderValue};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::path::PathBuf;
use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt,
    fs::Metadata,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
    time::{Duration, Instant},
};
use tokio::{io, sync::OwnedSemaphorePermit};
use tokio_util::sync::CancellationToken;

mod snapshot;
mod walk;

pub(super) use snapshot::ListSnapshotCache;
use snapshot::{
    LIST_CURSOR_UNAVAILABLE_MESSAGE, ListSnapshotBinding, ListSnapshotLookupError,
    ListSnapshotPage, ListSnapshotRequest, MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER,
    decode_list_cursor, list_snapshot_owner,
};
#[cfg(test)]
use snapshot::{
    LIST_SNAPSHOT_TTL, ListSnapshotRecord, ListSnapshotStore, MAX_CACHED_LIST_SNAPSHOT_BYTES,
    MAX_CACHED_LIST_SNAPSHOTS, MAX_CACHED_LIST_SNAPSHOTS_PER_OWNER, encode_list_cursor,
    list_snapshot_weight,
};
#[cfg(test)]
pub(super) use walk::collect_dir_entries;
pub(super) use walk::{
    CollectionByteBudget, DirectoryWalk, collect_dir_items, spawn_directory_blocking,
};

const INDEX_HTML: &str = include_str!("../../assets/index.html");
const UNSUPPORTED_FILENAME_MESSAGE: &str =
    "The folder contains an unsupported non-UTF-8 name. Rename it on Linux.";
pub(super) const DIRECTORY_CHANGED_DURING_WALK_MESSAGE: &str =
    "The folder changed during traversal. Try again.";
pub(super) const LIST_API_PATH: &str = "__dufs__/api/list";
const DEFAULT_LIST_PAGE_SIZE: usize = 200;
const MAX_LIST_PAGE_SIZE: usize = 500;
const MAX_LIST_SNAPSHOT_ENTRIES: usize = 100_000;
pub(super) const MAX_DIRECTORY_WALK_DEPTH: usize = 1_024;
pub(super) const MAX_DIRECTORY_WALK_WORKING_BYTES: usize = 32 * 1024 * 1024;

pub(super) type ListingResult<T> = std::result::Result<T, ListingError>;

pub(super) struct CancelOnDrop {
    cancellation: CancellationToken,
}

impl CancelOnDrop {
    pub(super) fn new(cancellation: CancellationToken) -> Self {
        Self { cancellation }
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Closed catalog of public failures produced while listing directories.
///
/// `ListingError::reason` is deliberately kept separate: it is diagnostic
/// context for logs and tests and must never select the HTTP problem exposed
/// to a client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::server) enum ListingProblem {
    UnsupportedFilename,
    DirectoryAccessForbidden,
    DirectoryChanged,
    DirectoryOperationTimeout,
    DirectoryOperationFailed,
    ServerStopping,
    DirectorySymlinkLoop,
    DirectoryListingEntryLimit,
    DirectoryWalkEntryLimit,
    DirectoryDepthLimit,
    DirectoryMemoryLimit,
    SearchResultLimit,
    ListSnapshotLimit,
    ListSnapshotAllocationFailed,
    DirectorySortLimit,
}

impl ListingProblem {
    pub(in crate::server) const fn status(self) -> StatusCode {
        match self {
            Self::UnsupportedFilename | Self::DirectoryChanged | Self::DirectorySymlinkLoop => {
                StatusCode::CONFLICT
            }
            Self::DirectoryAccessForbidden => StatusCode::FORBIDDEN,
            Self::DirectoryOperationTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::DirectoryOperationFailed => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ServerStopping => StatusCode::SERVICE_UNAVAILABLE,
            Self::DirectoryListingEntryLimit
            | Self::DirectoryWalkEntryLimit
            | Self::DirectoryDepthLimit
            | Self::DirectoryMemoryLimit
            | Self::SearchResultLimit
            | Self::ListSnapshotLimit
            | Self::ListSnapshotAllocationFailed
            | Self::DirectorySortLimit => StatusCode::PAYLOAD_TOO_LARGE,
        }
    }

    pub(in crate::server) const fn public_message(self) -> &'static str {
        match self {
            Self::UnsupportedFilename => UNSUPPORTED_FILENAME_MESSAGE,
            Self::DirectoryAccessForbidden => "Forbidden",
            Self::DirectoryChanged => DIRECTORY_CHANGED_DURING_WALK_MESSAGE,
            Self::DirectoryOperationTimeout => "Directory operation timed out",
            Self::DirectoryOperationFailed => "Directory operation failed",
            Self::ServerStopping => "Server is stopping",
            Self::DirectorySymlinkLoop => "Directory symlink loop detected",
            Self::DirectoryListingEntryLimit => "Directory listing exceeded its entry limit",
            Self::DirectoryWalkEntryLimit => "Directory operation exceeded its entry limit",
            Self::DirectoryDepthLimit => "Directory traversal exceeded its depth limit",
            Self::DirectoryMemoryLimit => "Directory traversal exceeds the memory limit",
            Self::SearchResultLimit => "Search results exceed the memory limit",
            Self::ListSnapshotLimit | Self::ListSnapshotAllocationFailed => {
                "Directory listing exceeds the snapshot capacity"
            }
            Self::DirectorySortLimit => "Directory sorting exceeds the memory limit",
        }
    }

    pub(in crate::server) const fn code(self) -> ErrorCode {
        match self {
            Self::UnsupportedFilename => ErrorCode::UNSUPPORTED_FILENAME,
            Self::DirectoryAccessForbidden => ErrorCode::DIRECTORY_ACCESS_FORBIDDEN,
            Self::DirectoryChanged => ErrorCode::DIRECTORY_CHANGED,
            Self::DirectoryOperationTimeout => ErrorCode::DIRECTORY_OPERATION_TIMEOUT,
            Self::ServerStopping => ErrorCode::SERVER_STOPPING,
            Self::DirectorySymlinkLoop => ErrorCode::DIRECTORY_SYMLINK_LOOP,
            Self::DirectoryListingEntryLimit | Self::DirectoryWalkEntryLimit => {
                ErrorCode::DIRECTORY_ENTRY_LIMIT
            }
            Self::DirectoryDepthLimit => ErrorCode::DIRECTORY_DEPTH_LIMIT,
            Self::DirectoryMemoryLimit => ErrorCode::DIRECTORY_MEMORY_LIMIT,
            Self::SearchResultLimit | Self::ListSnapshotAllocationFailed => {
                ErrorCode::SEARCH_RESULT_LIMIT
            }
            Self::ListSnapshotLimit => ErrorCode::LIST_SNAPSHOT_LIMIT,
            Self::DirectorySortLimit => ErrorCode::DIRECTORY_SORT_LIMIT,
            Self::DirectoryOperationFailed => ErrorCode::DIRECTORY_OPERATION_FAILED,
        }
    }

    pub(in crate::server) const fn recovery(self) -> RecoveryAdvice {
        match self {
            Self::UnsupportedFilename | Self::DirectoryChanged | Self::DirectorySymlinkLoop => {
                RecoveryAdvice::RefreshTarget
            }
            Self::DirectoryOperationTimeout => RecoveryAdvice::Retry,
            Self::ServerStopping => RecoveryAdvice::RetryAfterSeconds(1),
            Self::DirectoryAccessForbidden
            | Self::DirectoryOperationFailed
            | Self::DirectoryListingEntryLimit
            | Self::DirectoryWalkEntryLimit
            | Self::DirectoryDepthLimit
            | Self::DirectoryMemoryLimit
            | Self::SearchResultLimit
            | Self::ListSnapshotLimit
            | Self::ListSnapshotAllocationFailed
            | Self::DirectorySortLimit => RecoveryAdvice::None,
        }
    }
}

#[derive(Debug)]
pub(super) struct ListingError {
    pub(super) operation: &'static str,
    pub(super) relative_path: String,
    pub(super) reason: String,
    pub(super) problem: ListingProblem,
}

impl ListingError {
    pub(super) fn unsupported_name(operation: &'static str, path: &Path, root: &Path) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: "unsupported_filename_encoding".to_string(),
            problem: ListingProblem::UnsupportedFilename,
        }
    }

    pub(super) fn io(
        operation: &'static str,
        path: &Path,
        root: &Path,
        error: &std::io::Error,
    ) -> Self {
        let problem = match error.kind() {
            std::io::ErrorKind::PermissionDenied => ListingProblem::DirectoryAccessForbidden,
            std::io::ErrorKind::InvalidData => ListingProblem::UnsupportedFilename,
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                ListingProblem::DirectoryChanged
            }
            std::io::ErrorKind::TimedOut => ListingProblem::DirectoryOperationTimeout,
            _ => ListingProblem::DirectoryOperationFailed,
        };
        let raw_os_error = error
            .raw_os_error()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: format!("io_kind={:?} raw_os_error={raw_os_error}", error.kind()),
            problem,
        }
    }

    pub(super) fn cancelled(operation: &'static str, path: &Path, root: &Path) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: "server_stopping".to_string(),
            problem: ListingProblem::ServerStopping,
        }
    }

    pub(super) fn symlink_loop(operation: &'static str, path: &Path, root: &Path) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: "ancestor_symlink_loop".to_string(),
            problem: ListingProblem::DirectorySymlinkLoop,
        }
    }

    pub(super) fn limit(
        operation: &'static str,
        path: &Path,
        root: &Path,
        reason: &'static str,
        problem: ListingProblem,
    ) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: reason.to_string(),
            problem,
        }
    }

    pub(super) fn invariant(operation: &'static str, path: &Path, root: &Path) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: "path_outside_expected_base".to_string(),
            problem: ListingProblem::DirectoryOperationFailed,
        }
    }
}

impl fmt::Display for ListingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "directory operation failed operation={} path=\"{}\" reason={}",
            self.operation, self.relative_path, self.reason
        )
    }
}

impl std::error::Error for ListingError {}

impl Server {
    pub(super) async fn handle_list_api(
        &self,
        account: &str,
        query_params: &HashMap<String, String>,
        res: &mut Response,
    ) -> Result<()> {
        let owner = list_snapshot_owner(account);
        let logical_path = query_params.get("path").map(String::as_str).unwrap_or("/");
        let Some(path) = self
            .content
            .path_policy
            .parse_list_target(logical_path)
            .map(super::path_policy::RootedPath::into_path_buf)
        else {
            respond_list_api_problem(
                res,
                StatusCode::BAD_REQUEST,
                ErrorCode::INVALID_LIST_PATH,
                "Invalid list path",
                RecoveryAdvice::None,
            )?;
            return Ok(());
        };
        let sort = query_params
            .get("sort")
            .map(String::as_str)
            .filter(|value| matches!(*value, "name" | "mtime" | "size"))
            .unwrap_or("name")
            .to_string();
        let order = query_params
            .get("order")
            .map(String::as_str)
            .filter(|value| matches!(*value, "asc" | "desc"))
            .unwrap_or("asc")
            .to_string();
        let query = query_params.get("q").cloned().unwrap_or_default();
        if query.chars().count() > 128 {
            respond_list_api_problem(
                res,
                StatusCode::BAD_REQUEST,
                ErrorCode::SEARCH_QUERY_TOO_LONG,
                "Search query is too long",
                RecoveryAdvice::None,
            )?;
            return Ok(());
        }
        let limit = match query_params.get("limit") {
            Some(value) => match value.parse::<usize>() {
                Ok(value) if (1..=MAX_LIST_PAGE_SIZE).contains(&value) => value,
                _ => {
                    respond_list_api_problem(
                        res,
                        StatusCode::BAD_REQUEST,
                        ErrorCode::INVALID_LIST_LIMIT,
                        "Invalid list limit",
                        RecoveryAdvice::None,
                    )?;
                    return Ok(());
                }
            },
            None => DEFAULT_LIST_PAGE_SIZE,
        };
        let cursor = match query_params.get("cursor") {
            Some(value) => match decode_list_cursor(value) {
                Ok(cursor) => Some(cursor),
                _ => {
                    respond_list_api_problem(
                        res,
                        StatusCode::BAD_REQUEST,
                        ErrorCode::INVALID_LIST_CURSOR,
                        "Invalid list cursor",
                        RecoveryAdvice::None,
                    )?;
                    return Ok(());
                }
            },
            None => None,
        };

        let before = match self.content.rooted_fs.metadata(&path).await {
            Ok(metadata) if metadata.is_dir() => DirectorySnapshot::from_metadata(&metadata),
            Ok(_) => {
                respond_list_api_problem(
                    res,
                    StatusCode::CONFLICT,
                    ErrorCode::LIST_PATH_NOT_DIRECTORY,
                    "List path is not a directory",
                    RecoveryAdvice::RefreshTarget,
                )?;
                return Ok(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                respond_list_api_problem(
                    res,
                    StatusCode::NOT_FOUND,
                    ErrorCode::LIST_PATH_NOT_FOUND,
                    "List path was not found",
                    RecoveryAdvice::RefreshTarget,
                )?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };

        if let Some(cursor) = cursor {
            let request = ListSnapshotRequest {
                owner,
                path: &path,
                directory: before,
                sort: &sort,
                order: &order,
                query: &query,
                limit,
            };
            let page = match self
                .content
                .list_snapshot_cache
                .page(&cursor, request, Instant::now())
            {
                Ok(page) => page,
                Err(ListSnapshotLookupError::InvalidBinding) => {
                    respond_list_api_problem(
                        res,
                        StatusCode::BAD_REQUEST,
                        ErrorCode::INVALID_LIST_CURSOR,
                        "Invalid list cursor",
                        RecoveryAdvice::None,
                    )?;
                    return Ok(());
                }
                Err(ListSnapshotLookupError::DirectoryChanged) => {
                    respond_list_api_problem(
                        res,
                        StatusCode::CONFLICT,
                        ErrorCode::DIRECTORY_CHANGED,
                        "Directory changed; restart listing",
                        RecoveryAdvice::RefreshTarget,
                    )?;
                    return Ok(());
                }
                Err(ListSnapshotLookupError::Unavailable) => {
                    respond_list_api_problem(
                        res,
                        StatusCode::CONFLICT,
                        ErrorCode::LIST_CURSOR_UNAVAILABLE,
                        LIST_CURSOR_UNAVAILABLE_MESSAGE,
                        RecoveryAdvice::RefreshTarget,
                    )?;
                    return Ok(());
                }
            };
            return write_list_response(res, page);
        }

        let cancellation = CancellationToken::new();
        let _cancel_on_drop = CancelOnDrop::new(cancellation.clone());

        let list_permit = match self.admission.search_slots.clone().try_acquire_owned() {
            Ok(permit) => Arc::new(permit),
            Err(_) => {
                warn!(
                    "Listing rejected reason=concurrency_limit limit={}",
                    self.content.args.max_concurrent_searches
                );
                respond_list_api_problem(
                    res,
                    StatusCode::TOO_MANY_REQUESTS,
                    ErrorCode::DIRECTORY_OPERATION_LIMIT,
                    "Too many directory operations are running",
                    RecoveryAdvice::RetryAfterSeconds(1),
                )?;
                return Ok(());
            }
        };

        let paths = if query.is_empty() {
            let rooted_fs = self.content.rooted_fs.clone();
            let serve_path = self.content.args.serve_path.clone();
            let list_path = path.clone();
            let sort_for_task = sort.clone();
            let order_for_task = order.clone();
            let running = self.lifecycle.running.clone();
            let max_duration = Duration::from_secs(self.content.args.request_timeout);
            let result = spawn_directory_blocking(
                &self.lifecycle.work_tasks,
                Some(list_permit.clone()),
                move || {
                    collect_list_snapshot_blocking(
                        &rooted_fs,
                        &list_path,
                        ListSnapshotOptions {
                            serve_path: &serve_path,
                            max_entries: MAX_LIST_SNAPSHOT_ENTRIES,
                            max_bytes: MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER,
                            sort: &sort_for_task,
                            order: &order_for_task,
                            running: &running,
                            cancellation: &cancellation,
                            max_duration,
                        },
                    )
                },
            )
            .await
            .map_err(std::io::Error::other)?;
            match result {
                Ok(paths) => paths,
                Err(error) => {
                    respond_list_api_listing_error(res, &error)?;
                    return Ok(());
                }
            }
        } else {
            let search = query.to_lowercase();
            match self
                .search_dir(
                    &path,
                    &search,
                    &sort,
                    &order,
                    list_permit.clone(),
                    cancellation.clone(),
                )
                .await
            {
                Ok(paths) => paths,
                Err(error) => {
                    respond_list_api_listing_error(res, &error)?;
                    return Ok(());
                }
            }
        };

        let after = match directory_snapshot_after_walk(
            self.content.rooted_fs.metadata(&path).await,
            &path,
            &self.content.args.serve_path,
        ) {
            Ok(after) => after,
            Err(error) => {
                respond_list_api_listing_error(res, &error)?;
                return Ok(());
            }
        };
        if after != before {
            respond_list_api_problem(
                res,
                StatusCode::CONFLICT,
                ErrorCode::DIRECTORY_CHANGED,
                "Directory changed; restart listing",
                RecoveryAdvice::RefreshTarget,
            )?;
            return Ok(());
        }

        let page = if paths.len() > limit {
            let binding = ListSnapshotBinding {
                owner,
                path,
                directory: before,
                sort,
                order,
                query,
                limit,
            };
            match self.content.list_snapshot_cache.cache(
                binding,
                paths,
                &self.content.args.serve_path,
                Instant::now(),
            ) {
                Ok(page) => page,
                Err(error) => {
                    respond_list_api_listing_error(res, &error)?;
                    return Ok(());
                }
            }
        } else {
            ListSnapshotPage::from_vec(paths, None)
        };
        write_list_response(res, page)
    }

    pub(super) async fn handle_ls_dir(
        &self,
        path: &Path,
        exist: bool,
        _query_params: &HashMap<String, String>,
        head_only: bool,
        session: SessionInfo,
        res: &mut Response,
    ) -> Result<()> {
        self.send_index(
            path,
            IndexOptions {
                exist,
                head_only,
                session,
            },
            res,
        )
    }

    pub(super) async fn handle_search_dir(
        &self,
        path: &Path,
        query_params: &HashMap<String, String>,
        head_only: bool,
        session: SessionInfo,
        res: &mut Response,
    ) -> Result<()> {
        let search = query_params
            .get("q")
            .ok_or_else(|| anyhow!("invalid q"))?
            .to_lowercase();
        if search.is_empty() {
            return self
                .handle_ls_dir(path, true, query_params, head_only, session, res)
                .await;
        }

        self.send_index(
            path,
            IndexOptions {
                exist: true,
                head_only,
                session,
            },
            res,
        )
    }

    async fn search_dir(
        &self,
        path: &Path,
        search: &str,
        sort: &str,
        order: &str,
        permit: Arc<OwnedSemaphorePermit>,
        cancellation: CancellationToken,
    ) -> ListingResult<Vec<PathItem>> {
        let started = Instant::now();
        let max_duration = Duration::from_secs(self.content.args.request_timeout);
        let search = search.to_owned();
        let base_path = path.to_path_buf();
        let serve_path = self.content.args.serve_path.clone();
        let mut paths = collect_dir_items(
            DirectoryWalk {
                work_tasks: self.lifecycle.work_tasks.clone(),
                running: self.lifecycle.running.clone(),
                cancellation: cancellation.clone(),
                path: path.to_path_buf(),
                serve_path: self.content.args.serve_path.clone(),
                rooted_fs: self.content.rooted_fs.clone(),
                max_entries: self.content.args.max_search_entries,
                max_depth: MAX_DIRECTORY_WALK_DEPTH,
                max_working_bytes: MAX_DIRECTORY_WALK_WORKING_BYTES,
                max_duration,
                permit: Some(permit.clone()),
            },
            move |_entry, name| Ok(case_folded_contains(name, &search)),
            move |entry| pathitem_from_rooted_entry(entry, &base_path, &serve_path),
            path_item_heap_bytes,
            Some(CollectionByteBudget {
                max_bytes: MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER,
                operation: "search_result",
                reason: "result_memory_budget",
                problem: ListingProblem::SearchResultLimit,
                allocation_problem: ListingProblem::SearchResultLimit,
            }),
        )
        .await?;

        let sort = sort.to_owned();
        let order = order.to_owned();
        let running = self.lifecycle.running.clone();
        let cancellation_for_sort = cancellation;
        let sort_path = path.to_path_buf();
        let serve_path = self.content.args.serve_path.clone();
        spawn_directory_blocking(&self.lifecycle.work_tasks, Some(permit), move || {
            ensure_path_sort_budget(
                &paths,
                paths.capacity(),
                MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER,
                "search_sort",
                &sort_path,
                &serve_path,
            )?;
            sort_path_items_interruptibly(
                &mut paths,
                &sort,
                &order,
                &running,
                &cancellation_for_sort,
                started,
                max_duration,
                "search_sort",
                &sort_path,
                &serve_path,
            )?;
            shrink_paths_if_wasteful(&mut paths);
            Ok(paths)
        })
        .await
        .map_err(|error| ListingError {
            operation: "search_sort_worker",
            relative_path: safe_relative_path(path, &self.content.args.serve_path),
            reason: format!("worker_join_error={error}"),
            problem: ListingProblem::DirectoryOperationFailed,
        })?
    }

    fn send_index(&self, path: &Path, options: IndexOptions, res: &mut Response) -> Result<()> {
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
        res.headers_mut()
            .typed_insert(CacheControl::new().with_private().with_no_store());
        res.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        res.headers_mut()
            .insert("x-frame-options", HeaderValue::from_static("DENY"));
        res.headers_mut()
            .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        res.headers_mut().insert(
            "permissions-policy",
            HeaderValue::from_static(
                "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
            ),
        );
        res.headers_mut().insert(
            "content-security-policy",
            HeaderValue::from_static(
                "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
            ),
        );
        if options.head_only {
            return Ok(());
        }

        let href = format!(
            "/{}",
            strict_relative_path(
                path,
                &self.content.args.serve_path,
                "directory_page_path",
                &self.content.args.serve_path,
            )?
        );
        let data = IndexData {
            href,
            dir_exists: options.exist,
            user: options.session.user,
            csrf_token: options.session.csrf_token,
        };
        let index_data = STANDARD.encode(serde_json::to_string(&data)?);
        let output = INDEX_HTML
            .replace(
                "__ASSETS_PREFIX__",
                &format!("/{}", self.content.assets_prefix),
            )
            .replace("__INDEX_DATA__", &index_data);
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.body_mut() = body_full(output);
        Ok(())
    }
}

struct ListSnapshotOptions<'a> {
    serve_path: &'a Path,
    max_entries: usize,
    max_bytes: usize,
    sort: &'a str,
    order: &'a str,
    running: &'a AtomicBool,
    cancellation: &'a CancellationToken,
    max_duration: Duration,
}

#[derive(Clone, Copy)]
enum ListScanStop {
    Cancelled,
    EntryBudget,
    Timeout,
}

fn collect_list_snapshot_blocking(
    rooted_fs: &RootedFs,
    path: &Path,
    options: ListSnapshotOptions<'_>,
) -> ListingResult<Vec<PathItem>> {
    let ListSnapshotOptions {
        serve_path,
        max_entries,
        max_bytes,
        sort,
        order,
        running,
        cancellation,
        max_duration,
    } = options;
    let mut paths = Vec::new();
    let mut visited = 0usize;
    let mut heap_bytes = 0usize;
    let started = Instant::now();
    let stop = std::cell::Cell::new(None);
    let listing_error = std::cell::RefCell::new(None);
    let visit_result = rooted_fs.visit_dir_blocking_bounded(
        path,
        |_| {
            if cancellation.is_cancelled() || !running.load(atomic::Ordering::SeqCst) {
                stop.set(Some(ListScanStop::Cancelled));
                return false;
            }
            if started.elapsed() >= max_duration {
                stop.set(Some(ListScanStop::Timeout));
                return false;
            }
            visited = visited.saturating_add(1);
            if visited > max_entries {
                stop.set(Some(ListScanStop::EntryBudget));
                return false;
            }
            true
        },
        |entry| {
            if cancellation.is_cancelled() || !running.load(atomic::Ordering::SeqCst) {
                stop.set(Some(ListScanStop::Cancelled));
                return Ok(false);
            }
            if started.elapsed() >= max_duration {
                stop.set(Some(ListScanStop::Timeout));
                return Ok(false);
            }
            let Some(base_name) = entry.file_name.to_str() else {
                *listing_error.borrow_mut() = Some(ListingError::unsupported_name(
                    "list_snapshot_filename",
                    &entry.path,
                    serve_path,
                ));
                return Ok(false);
            };
            if is_internal_name(base_name) {
                return Ok(true);
            }
            let entry_path = entry.path.clone();
            match pathitem_from_rooted_entry(entry, path, serve_path) {
                Ok(item) => {
                    let item_heap_weight = path_item_heap_bytes(&item);
                    let proposed_heap_bytes = heap_bytes.saturating_add(item_heap_weight);
                    let minimum = collection_allocation_bytes(
                        paths.len().saturating_add(1),
                        std::mem::size_of::<PathItem>(),
                        proposed_heap_bytes,
                    );
                    if minimum > max_bytes {
                        *listing_error.borrow_mut() = Some(ListingError::limit(
                            "list_snapshot",
                            path,
                            serve_path,
                            "snapshot_memory_budget",
                            ListingProblem::ListSnapshotLimit,
                        ));
                        return Ok(false);
                    }
                    match try_reserve_bounded_vec_slot(
                        &mut paths,
                        std::mem::size_of::<Vec<PathItem>>().saturating_add(proposed_heap_bytes),
                        max_bytes,
                    ) {
                        Ok(()) => {}
                        Err(BoundedReserveError::Budget) => {
                            *listing_error.borrow_mut() = Some(ListingError::limit(
                                "list_snapshot",
                                path,
                                serve_path,
                                "snapshot_memory_budget",
                                ListingProblem::ListSnapshotLimit,
                            ));
                            return Ok(false);
                        }
                        Err(BoundedReserveError::Allocation) => {
                            *listing_error.borrow_mut() = Some(ListingError::limit(
                                "list_snapshot",
                                &entry_path,
                                serve_path,
                                "result_allocation_failed",
                                ListingProblem::ListSnapshotAllocationFailed,
                            ));
                            return Ok(false);
                        }
                    }
                    let allocated = collection_allocation_bytes(
                        paths.capacity(),
                        std::mem::size_of::<PathItem>(),
                        proposed_heap_bytes,
                    );
                    if allocated > max_bytes {
                        *listing_error.borrow_mut() = Some(ListingError::limit(
                            "list_snapshot",
                            path,
                            serve_path,
                            "snapshot_memory_budget",
                            ListingProblem::ListSnapshotLimit,
                        ));
                        return Ok(false);
                    }
                    heap_bytes = proposed_heap_bytes;
                    paths.push(item);
                }
                Err(error) => {
                    *listing_error.borrow_mut() = Some(error);
                    return Ok(false);
                }
            }
            Ok(true)
        },
    );

    if let Some(error) = listing_error.into_inner() {
        return Err(error);
    }
    if let Err(error) = visit_result {
        return Err(ListingError::io("list_snapshot", path, serve_path, &error));
    }
    match stop.get() {
        Some(ListScanStop::Cancelled) => {
            return Err(ListingError::cancelled("list_snapshot", path, serve_path));
        }
        Some(ListScanStop::EntryBudget) => {
            return Err(ListingError::limit(
                "list_snapshot",
                path,
                serve_path,
                "entry_budget",
                ListingProblem::DirectoryListingEntryLimit,
            ));
        }
        Some(ListScanStop::Timeout) => {
            return Err(ListingError::limit(
                "list_snapshot",
                path,
                serve_path,
                "time_budget",
                ListingProblem::DirectoryOperationTimeout,
            ));
        }
        None => {}
    }

    ensure_path_sort_budget(
        &paths,
        paths.capacity(),
        max_bytes,
        "list_snapshot_sort",
        path,
        serve_path,
    )?;
    sort_path_items_interruptibly(
        &mut paths,
        sort,
        order,
        running,
        cancellation,
        started,
        max_duration,
        "list_snapshot_sort",
        path,
        serve_path,
    )?;
    shrink_paths_if_wasteful(&mut paths);
    Ok(paths)
}

fn case_folded_contains(value: &str, folded_query: &str) -> bool {
    if value.is_ascii() && folded_query.is_ascii() {
        let query = folded_query.as_bytes();
        return query.is_empty()
            || value
                .as_bytes()
                .windows(query.len())
                .any(|window| window.eq_ignore_ascii_case(query));
    }
    value.to_lowercase().contains(folded_query)
}

fn shrink_paths_if_wasteful(paths: &mut Vec<PathItem>) {
    const MINIMUM_SPARE_ITEMS: usize = 1_024;
    let spare = paths.capacity().saturating_sub(paths.len());
    if spare >= MINIMUM_SPARE_ITEMS
        && paths.capacity() > paths.len().saturating_add(paths.len() / 4)
    {
        paths.shrink_to_fit();
    }
}

fn compare_path_items(left: &PathItem, right: &PathItem, sort: &str, order: &str) -> Ordering {
    let ordering = match sort {
        "mtime" => left.sort_by_mtime(right),
        "size" => left.sort_by_size(right),
        _ => left.sort_by_name(right),
    };
    if order == "desc" {
        ordering.reverse()
    } else {
        ordering
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterruptibleSortFailure {
    Interrupted,
    Allocation,
}

fn stable_sort_by_interruptible<T, F, A>(
    values: &mut [T],
    mut compare: F,
    mut active: A,
) -> std::result::Result<(), InterruptibleSortFailure>
where
    F: FnMut(&T, &T) -> Ordering,
    A: FnMut() -> bool,
{
    const INTERRUPT_CHECK_INTERVAL: usize = 256;

    if !active() {
        return Err(InterruptibleSortFailure::Interrupted);
    }
    let len = values.len();
    if len < 2 {
        return Ok(());
    }

    // Sort indices instead of moving PathItems during every merge. This keeps
    // the operation stable, limits scratch memory to two usize arrays, and
    // gives cancellation/deadline checks between every bounded unit of work.
    let mut current = Vec::new();
    current
        .try_reserve_exact(len)
        .map_err(|_| InterruptibleSortFailure::Allocation)?;
    let mut work_since_check = 0usize;
    for index in 0..len {
        work_since_check += 1;
        if work_since_check >= INTERRUPT_CHECK_INTERVAL && !active() {
            return Err(InterruptibleSortFailure::Interrupted);
        }
        if work_since_check >= INTERRUPT_CHECK_INTERVAL {
            work_since_check = 0;
        }
        current.push(index);
    }
    let mut next = Vec::new();
    next.try_reserve_exact(len)
        .map_err(|_| InterruptibleSortFailure::Allocation)?;

    let mut width = 1usize;
    while width < len {
        next.clear();
        let run_width = width.saturating_mul(2);
        let mut start = 0usize;
        while start < len {
            let middle = start.saturating_add(width).min(len);
            let end = start.saturating_add(run_width).min(len);
            let mut left = start;
            let mut right = middle;
            while left < middle || right < end {
                work_since_check += 1;
                if work_since_check >= INTERRUPT_CHECK_INTERVAL && !active() {
                    return Err(InterruptibleSortFailure::Interrupted);
                }
                if work_since_check >= INTERRUPT_CHECK_INTERVAL {
                    work_since_check = 0;
                }
                let take_left = right == end
                    || (left < middle
                        && compare(&values[current[left]], &values[current[right]])
                            != Ordering::Greater);
                if take_left {
                    next.push(current[left]);
                    left += 1;
                } else {
                    next.push(current[right]);
                    right += 1;
                }
            }
            start = end;
        }
        std::mem::swap(&mut current, &mut next);
        width = run_width;
    }

    // Convert new-position -> old-position into old-position -> new-position,
    // then apply each permutation cycle in place. Equal keys retained their
    // original relative order in the merge above.
    next.clear();
    next.resize(len, 0);
    for (new_position, &old_position) in current.iter().enumerate() {
        work_since_check += 1;
        if work_since_check >= INTERRUPT_CHECK_INTERVAL && !active() {
            return Err(InterruptibleSortFailure::Interrupted);
        }
        if work_since_check >= INTERRUPT_CHECK_INTERVAL {
            work_since_check = 0;
        }
        next[old_position] = new_position;
    }
    for position in 0..len {
        while next[position] != position {
            work_since_check += 1;
            if work_since_check >= INTERRUPT_CHECK_INTERVAL && !active() {
                return Err(InterruptibleSortFailure::Interrupted);
            }
            if work_since_check >= INTERRUPT_CHECK_INTERVAL {
                work_since_check = 0;
            }
            let destination = next[position];
            values.swap(position, destination);
            next.swap(position, destination);
        }
    }
    if !active() {
        return Err(InterruptibleSortFailure::Interrupted);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn sort_path_items_interruptibly(
    paths: &mut [PathItem],
    sort: &str,
    order: &str,
    running: &AtomicBool,
    cancellation: &CancellationToken,
    started: Instant,
    max_duration: Duration,
    operation: &'static str,
    path: &Path,
    serve_root: &Path,
) -> ListingResult<()> {
    match stable_sort_by_interruptible(
        paths,
        |left, right| compare_path_items(left, right, sort, order),
        || {
            !cancellation.is_cancelled()
                && running.load(atomic::Ordering::Relaxed)
                && started.elapsed() < max_duration
        },
    ) {
        Ok(()) => Ok(()),
        Err(InterruptibleSortFailure::Interrupted) => {
            if !cancellation.is_cancelled() && running.load(atomic::Ordering::SeqCst) {
                Err(ListingError::limit(
                    operation,
                    path,
                    serve_root,
                    "time_budget",
                    ListingProblem::DirectoryOperationTimeout,
                ))
            } else {
                Err(ListingError::cancelled(operation, path, serve_root))
            }
        }
        Err(InterruptibleSortFailure::Allocation) => Err(ListingError::limit(
            operation,
            path,
            serve_root,
            "sort_allocation_failed",
            ListingProblem::DirectorySortLimit,
        )),
    }
}

fn pathitem_from_rooted_entry(
    entry: RootedDirEntry,
    base_path: &Path,
    serve_path: &Path,
) -> ListingResult<PathItem> {
    strict_file_name(&entry.path, "pathitem_filename", serve_path)?;
    let is_dir = entry.metadata.is_dir();
    let path_type = match (entry.is_symlink, is_dir) {
        (true, true) => PathType::SymlinkDir,
        (false, true) => PathType::Dir,
        (true, false) => PathType::SymlinkFile,
        (false, false) => PathType::File,
    };
    let mtime = match entry
        .metadata
        .modified()
        .ok()
        .or_else(|| entry.metadata.created().ok())
    {
        Some(value) => super::to_timestamp(&value),
        None => 0,
    };
    let size = if is_dir { 0 } else { entry.metadata.len() };
    let name = strict_relative_path(&entry.path, base_path, "pathitem_relative_path", serve_path)?;
    Ok(PathItem {
        path_type,
        sort_name: name.to_lowercase(),
        name,
        mtime,
        size,
    })
}

struct IndexOptions {
    exist: bool,
    head_only: bool,
    session: SessionInfo,
}

#[derive(Debug, Serialize)]
struct IndexData {
    href: String,
    dir_exists: bool,
    user: String,
    csrf_token: String,
}

#[derive(Debug, Serialize)]
struct ListResponse<'a> {
    paths: &'a [PathItem],
    next_cursor: Option<&'a str>,
}

fn write_list_response(res: &mut Response, page: ListSnapshotPage) -> Result<()> {
    let output = serde_json::to_vec(&ListResponse {
        paths: page.paths(),
        next_cursor: page.next_cursor.as_deref(),
    })?;
    res.headers_mut()
        .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
    res.headers_mut()
        .typed_insert(ContentLength(output.len() as u64));
    *res.body_mut() = body_full(output);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectorySnapshot {
    device: u64,
    inode: u64,
    mtime: i64,
    mtime_nanoseconds: i64,
    ctime: i64,
    ctime_nanoseconds: i64,
}

impl DirectorySnapshot {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mtime: metadata.mtime(),
            mtime_nanoseconds: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

fn directory_snapshot_after_walk(
    metadata: io::Result<Metadata>,
    path: &Path,
    serve_root: &Path,
) -> ListingResult<DirectorySnapshot> {
    let metadata = metadata
        .map_err(|error| ListingError::io("list_snapshot_after", path, serve_root, &error))?;
    if !metadata.is_dir() {
        return Err(ListingError::limit(
            "list_snapshot_after",
            path,
            serve_root,
            "directory_replaced_by_non_directory",
            ListingProblem::DirectoryChanged,
        ));
    }
    Ok(DirectorySnapshot::from_metadata(&metadata))
}

fn path_item_heap_bytes(item: &PathItem) -> usize {
    item.name
        .capacity()
        .saturating_add(item.sort_name.capacity())
}

fn collection_allocation_bytes(capacity: usize, item_size: usize, heap_bytes: usize) -> usize {
    std::mem::size_of::<Vec<()>>()
        .saturating_add(capacity.saturating_mul(item_size))
        .saturating_add(heap_bytes)
}

fn ensure_path_sort_budget(
    paths: &[PathItem],
    path_capacity: usize,
    max_bytes: usize,
    operation: &'static str,
    path: &Path,
    serve_root: &Path,
) -> ListingResult<()> {
    let heap_bytes = paths.iter().fold(0usize, |total, item| {
        total.saturating_add(path_item_heap_bytes(item))
    });
    let retained =
        collection_allocation_bytes(path_capacity, std::mem::size_of::<PathItem>(), heap_bytes);
    let scratch = paths
        .len()
        .saturating_mul(std::mem::size_of::<usize>())
        .saturating_mul(2);
    if retained.saturating_add(scratch) > max_bytes {
        return Err(ListingError::limit(
            operation,
            path,
            serve_root,
            "sort_memory_budget",
            ListingProblem::DirectorySortLimit,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BoundedReserveError {
    Budget,
    Allocation,
}

pub(super) fn try_reserve_bounded_vec_slot<T>(
    values: &mut Vec<T>,
    non_vector_bytes: usize,
    maximum_bytes: usize,
) -> Result<(), BoundedReserveError> {
    if values.len() < values.capacity() || std::mem::size_of::<T>() == 0 {
        return Ok(());
    }

    let item_bytes = std::mem::size_of::<T>();
    let old_allocation = values.capacity().saturating_mul(item_bytes);
    let available_for_new =
        maximum_bytes.saturating_sub(non_vector_bytes.saturating_add(old_allocation));
    let maximum_new_capacity = available_for_new / item_bytes;
    let required_capacity = values.len().saturating_add(1);
    if maximum_new_capacity < required_capacity {
        return Err(BoundedReserveError::Budget);
    }

    let preferred_capacity = if values.capacity() == 0 {
        required_capacity.max(4)
    } else {
        values.capacity().saturating_mul(2).max(required_capacity)
    };
    let requested_capacity = preferred_capacity.min(maximum_new_capacity);
    values
        .try_reserve_exact(requested_capacity.saturating_sub(values.len()))
        .map_err(|_| BoundedReserveError::Allocation)?;
    if values.capacity() > maximum_new_capacity {
        return Err(BoundedReserveError::Budget);
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
struct PathItem {
    path_type: PathType,
    #[serde(skip)]
    sort_name: String,
    name: String,
    mtime: u64,
    size: u64,
}

impl PathItem {
    fn sort_by_name(&self, other: &Self) -> Ordering {
        match self.path_type.cmp(&other.path_type) {
            Ordering::Equal => alphanumeric_sort::compare_str(&self.sort_name, &other.sort_name)
                .then_with(|| self.name.cmp(&other.name)),
            value => value,
        }
    }

    fn sort_by_mtime(&self, other: &Self) -> Ordering {
        match self.path_type.cmp(&other.path_type) {
            Ordering::Equal => self
                .mtime
                .cmp(&other.mtime)
                .then_with(|| self.sort_by_name(other)),
            value => value,
        }
    }

    fn sort_by_size(&self, other: &Self) -> Ordering {
        match self.path_type.cmp(&other.path_type) {
            Ordering::Equal => self
                .size
                .cmp(&other.size)
                .then_with(|| self.sort_by_name(other)),
            value => value,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Eq, PartialEq)]
enum PathType {
    Dir,
    SymlinkDir,
    File,
    SymlinkFile,
}

impl Ord for PathType {
    fn cmp(&self, other: &Self) -> Ordering {
        let to_value = |path_type: &Self| -> u8 {
            if matches!(path_type, Self::Dir | Self::SymlinkDir) {
                0
            } else {
                1
            }
        };
        to_value(self).cmp(&to_value(other))
    }
}

impl PartialOrd for PathType {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn respond_list_api_problem(
    res: &mut Response,
    status: StatusCode,
    code: ErrorCode,
    detail: &'static str,
    recovery: RecoveryAdvice,
) -> Result<()> {
    render_problem(
        res,
        &ApiError::new(status, code, detail).with_recovery(recovery),
    )
}

fn respond_list_api_listing_error(res: &mut Response, error: &ListingError) -> Result<()> {
    error!("{error}");
    respond_list_api_problem(
        res,
        error.problem.status(),
        error.problem.code(),
        error.problem.public_message(),
        error.problem.recovery(),
    )
}

pub(super) fn strict_file_name<'a>(
    path: &'a Path,
    operation: &'static str,
    serve_root: &Path,
) -> ListingResult<&'a str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ListingError::unsupported_name(operation, path, serve_root))
}

fn strict_relative_path(
    path: &Path,
    base_path: &Path,
    operation: &'static str,
    serve_root: &Path,
) -> ListingResult<String> {
    let relative = path
        .strip_prefix(base_path)
        .map_err(|_| ListingError::invariant(operation, path, serve_root))?;
    relative
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| ListingError::unsupported_name(operation, path, serve_root))
}

fn safe_relative_path(path: &Path, serve_root: &Path) -> String {
    let Ok(relative) = path.strip_prefix(serve_root) else {
        return "<outside-root>".to_string();
    };
    if relative.as_os_str().is_empty() {
        return ".".to_string();
    }
    escape_path_bytes(relative.as_os_str().as_bytes())
}

fn escape_path_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut escaped = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'/' => escaped.push('/'),
            b'\\' => escaped.push_str("\\\\"),
            b'"' => escaped.push_str("\\\""),
            0x20..=0x7e => escaped.push(char::from(byte)),
            _ => {
                let _ = write!(escaped, "\\x{byte:02x}");
            }
        }
    }
    escaped
}

#[cfg(test)]
mod tests;
