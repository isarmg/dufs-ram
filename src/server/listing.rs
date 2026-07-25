use super::{
    BUF_SIZE, ContentDispositionFallback, Response, Server,
    disk_space::{DiskSpaceReservation, DiskSpaceTracker},
    rooted_fs::{RootedDirEntry, RootedFs},
    set_content_disposition, status_bad_request, status_error,
    upload::is_upload_temp_name,
};
use crate::{auth::SessionInfo, http_utils::body_full, utils::glob};

use anyhow::{Result, anyhow};
use async_deflate_zip::{Compression, WriterOptions, ZipError, ZipWriter};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use futures_util::TryStreamExt;
use headers::{CacheControl, ContentLength, ContentType, HeaderMapExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    StatusCode,
    body::{Body, Frame, SizeHint},
    header::HeaderValue,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fmt,
    fs::Metadata,
    os::fd::AsFd,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    fs::File,
    io::{self, AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, SeekFrom},
    sync::{OwnedSemaphorePermit, mpsc},
};
use tokio_util::{io::ReaderStream, task::TaskTracker};

const INDEX_HTML: &str = include_str!("../../assets/index.html");
const UNSUPPORTED_FILENAME_MESSAGE: &str = "目录包含不受支持的非 UTF-8 名称，请在 Linux 中重命名";
const DIRECTORY_CHANGED_DURING_WALK_MESSAGE: &str = "目录在遍历期间发生变化，请重试";
pub(super) const LIST_API_PATH: &str = "__dufs__/api/list";
const DEFAULT_LIST_PAGE_SIZE: usize = 200;
const MAX_LIST_PAGE_SIZE: usize = 500;

type ListingResult<T> = std::result::Result<T, ListingError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ZipWriteFailure {
    OutputLimit,
    InsufficientStorage,
}

impl fmt::Display for ZipWriteFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputLimit => formatter.write_str("ZIP output size limit exceeded"),
            Self::InsufficientStorage => {
                formatter.write_str("ZIP output would consume protected free disk space")
            }
        }
    }
}

impl std::error::Error for ZipWriteFailure {}

struct BoundedZipWriter<W> {
    inner: W,
    tracker: DiskSpaceTracker,
    pending_write: Option<DiskSpaceReservation>,
    written: u64,
    max_output_size: u64,
    minimum_free: u64,
}

impl<W> BoundedZipWriter<W> {
    fn new(inner: W, tracker: DiskSpaceTracker, max_output_size: u64, minimum_free: u64) -> Self {
        Self {
            inner,
            tracker,
            pending_write: None,
            written: 0,
            max_output_size,
            minimum_free,
        }
    }

    fn written(&self) -> u64 {
        self.written
    }

    fn into_inner(self) -> W {
        let Self {
            inner,
            pending_write,
            ..
        } = self;
        drop(pending_write);
        inner
    }
}

impl<W: AsyncWrite + AsFd + Unpin> AsyncWrite for BoundedZipWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buffer.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let requested = match u64::try_from(buffer.len()) {
            Ok(requested) => requested,
            Err(_) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ZIP write length does not fit in u64",
                )));
            }
        };

        if let Some(reservation) = &this.pending_write {
            if requested > reservation.remaining() {
                this.pending_write.take();
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ZIP writer was repolled with a larger buffer",
                )));
            }
        } else {
            let Some(end) = this.written.checked_add(requested) else {
                return Poll::Ready(Err(io::Error::other(ZipWriteFailure::OutputLimit)));
            };
            if end > this.max_output_size {
                return Poll::Ready(Err(io::Error::other(ZipWriteFailure::OutputLimit)));
            }
            match this
                .tracker
                .reserve(&this.inner, requested, this.minimum_free)
            {
                Ok(Some(reservation)) => this.pending_write = Some(reservation),
                Ok(None) => {
                    return Poll::Ready(Err(io::Error::other(
                        ZipWriteFailure::InsufficientStorage,
                    )));
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }

        match Pin::new(&mut this.inner).poll_write(context, buffer) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(bytes_written)) => {
                this.pending_write.take();
                let written = match u64::try_from(bytes_written) {
                    Ok(written) => written,
                    Err(_) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ZIP write result does not fit in u64",
                        )));
                    }
                };
                let Some(total_written) = this.written.checked_add(written) else {
                    return Poll::Ready(Err(io::Error::other(ZipWriteFailure::OutputLimit)));
                };
                if total_written > this.max_output_size {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ZIP writer reported more bytes than the reserved output budget",
                    )));
                }
                this.written = total_written;
                Poll::Ready(Ok(bytes_written))
            }
            Poll::Ready(Err(error)) => {
                this.pending_write.take();
                Poll::Ready(Err(error))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

pin_project_lite::pin_project! {
    struct PermitBody<B> {
        #[pin]
        inner: B,
        permit: Option<Arc<OwnedSemaphorePermit>>,
    }
}

impl<B> PermitBody<B> {
    fn new(inner: B, permit: Arc<OwnedSemaphorePermit>) -> Self {
        Self {
            inner,
            permit: Some(permit),
        }
    }
}

impl<B: Body> Body for PermitBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(None) => {
                this.permit.take();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                this.permit.take();
                Poll::Ready(Some(Err(error)))
            }
            result => result,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Debug)]
struct ListingError {
    operation: &'static str,
    relative_path: String,
    reason: String,
    status: StatusCode,
    public_message: &'static str,
}

impl ListingError {
    fn unsupported_name(operation: &'static str, path: &Path, root: &Path) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: "unsupported_filename_encoding".to_string(),
            status: StatusCode::CONFLICT,
            public_message: UNSUPPORTED_FILENAME_MESSAGE,
        }
    }

    fn io(operation: &'static str, path: &Path, root: &Path, error: &std::io::Error) -> Self {
        let status = match error.kind() {
            std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            std::io::ErrorKind::InvalidData => StatusCode::CONFLICT,
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                StatusCode::CONFLICT
            }
            std::io::ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let public_message = match error.kind() {
            std::io::ErrorKind::PermissionDenied => "Forbidden",
            std::io::ErrorKind::InvalidData => UNSUPPORTED_FILENAME_MESSAGE,
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                DIRECTORY_CHANGED_DURING_WALK_MESSAGE
            }
            std::io::ErrorKind::TimedOut => "Directory operation timed out",
            _ => "Directory operation failed",
        };
        let raw_os_error = error
            .raw_os_error()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: format!("io_kind={:?} raw_os_error={raw_os_error}", error.kind()),
            status,
            public_message,
        }
    }

    fn cancelled(operation: &'static str, path: &Path, root: &Path) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: "server_stopping".to_string(),
            status: StatusCode::SERVICE_UNAVAILABLE,
            public_message: "Server is stopping",
        }
    }

    fn limit(
        operation: &'static str,
        path: &Path,
        root: &Path,
        reason: &'static str,
        status: StatusCode,
        public_message: &'static str,
    ) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: reason.to_string(),
            status,
            public_message,
        }
    }

    fn archive(operation: &'static str, path: &Path, root: &Path) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: "archive_writer_error".to_string(),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            public_message: "Failed to create ZIP archive",
        }
    }

    fn invariant(operation: &'static str, path: &Path, root: &Path) -> Self {
        Self {
            operation,
            relative_path: safe_relative_path(path, root),
            reason: "path_outside_expected_base".to_string(),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            public_message: "Directory operation failed",
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
        query_params: &HashMap<String, String>,
        res: &mut Response,
    ) -> Result<()> {
        let logical_path = query_params.get("path").map(String::as_str).unwrap_or("/");
        let Some(path) = self.resolve_list_path(logical_path) else {
            status_bad_request(res, "Invalid list path");
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
            status_bad_request(res, "Search query is too long");
            return Ok(());
        }
        let limit = match query_params.get("limit") {
            Some(value) => match value.parse::<usize>() {
                Ok(value) if (1..=MAX_LIST_PAGE_SIZE).contains(&value) => value,
                _ => {
                    status_bad_request(res, "Invalid list limit");
                    return Ok(());
                }
            },
            None => DEFAULT_LIST_PAGE_SIZE,
        };
        let cursor = match query_params.get("cursor") {
            Some(value) => match decode_list_cursor(value) {
                Ok(cursor)
                    if cursor.sort == sort && cursor.order == order && cursor.query == query =>
                {
                    Some(cursor)
                }
                _ => {
                    status_bad_request(res, "Invalid list cursor");
                    return Ok(());
                }
            },
            None => None,
        };

        let list_permit = match self.search_slots.clone().try_acquire_owned() {
            Ok(permit) => Arc::new(permit),
            Err(_) => {
                warn!(
                    "Listing rejected reason=concurrency_limit limit={}",
                    self.args.max_concurrent_searches
                );
                status_error(
                    res,
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many directory operations are running",
                );
                return Ok(());
            }
        };

        let before = match self.rooted_fs.metadata(&path).await {
            Ok(metadata) if metadata.is_dir() => DirectorySnapshot::from_metadata(&metadata),
            Ok(_) => {
                status_error(res, StatusCode::CONFLICT, "List path is not a directory");
                return Ok(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                super::status_not_found(res);
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.directory != before)
        {
            status_error(
                res,
                StatusCode::CONFLICT,
                "Directory changed; restart listing",
            );
            return Ok(());
        }

        let last = cursor.as_ref().map(|cursor| cursor.last.to_path_item());
        let mut paths = if query.is_empty() {
            let rooted_fs = self.rooted_fs.clone();
            let hidden = self.args.hidden.clone();
            let serve_path = self.args.serve_path.clone();
            let list_path = path.clone();
            let sort_for_task = sort.clone();
            let order_for_task = order.clone();
            let running = self.running.clone();
            let max_duration = Duration::from_secs(self.args.request_timeout);
            let result =
                spawn_directory_blocking(&self.work_tasks, Some(list_permit.clone()), move || {
                    collect_list_page_blocking(
                        &rooted_fs,
                        &list_path,
                        ListPageOptions {
                            serve_path: &serve_path,
                            hidden: &hidden,
                            last: last.as_ref(),
                            limit,
                            sort: &sort_for_task,
                            order: &order_for_task,
                            running: &running,
                            max_duration,
                        },
                    )
                })
                .await
                .map_err(std::io::Error::other)?;
            match result {
                Ok(paths) => paths,
                Err(error) => {
                    respond_listing_error(res, &error);
                    return Ok(());
                }
            }
        } else {
            let search = query.to_lowercase();
            let mut matches = match self.search_dir(&path, &search, list_permit.clone()).await {
                Ok(paths) => paths,
                Err(error) => {
                    respond_listing_error(res, &error);
                    return Ok(());
                }
            };
            matches.retain(|item| {
                last.as_ref().is_none_or(|last| {
                    compare_path_items(item, last, &sort, &order) == Ordering::Greater
                })
            });
            matches.sort_by(|left, right| compare_path_items(left, right, &sort, &order));
            matches.truncate(limit.saturating_add(1));
            matches
        };

        let after = match directory_snapshot_after_walk(
            self.rooted_fs.metadata(&path).await,
            &path,
            &self.args.serve_path,
        ) {
            Ok(after) => after,
            Err(error) => {
                respond_listing_error(res, &error);
                return Ok(());
            }
        };
        if after != before {
            status_error(
                res,
                StatusCode::CONFLICT,
                "Directory changed; restart listing",
            );
            return Ok(());
        }

        let has_more = paths.len() > limit;
        paths.truncate(limit);
        let next_cursor = if has_more {
            paths.last().map(|last| {
                encode_list_cursor(&ListCursor {
                    version: 1,
                    directory: before,
                    sort: sort.clone(),
                    order: order.clone(),
                    query: query.clone(),
                    last: CursorItem::from(last),
                })
            })
        } else {
            None
        };
        let output = serde_json::to_vec(&ListResponse { paths, next_cursor })?;
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.body_mut() = body_full(output);
        Ok(())
    }

    fn resolve_list_path(&self, logical_path: &str) -> Option<PathBuf> {
        let relative = logical_path.strip_prefix('/')?;
        if relative.is_empty() {
            return Some(self.args.serve_path.clone());
        }
        let mut path = self.args.serve_path.clone();
        for component in relative.split('/') {
            if component.is_empty()
                || matches!(component, "." | "..")
                || component.contains('\0')
                || is_upload_temp_name(component)
            {
                return None;
            }
            path.push(component);
        }
        if relative
            .split('/')
            .next()
            .is_some_and(|component| self.is_reserved_internal_component(component))
        {
            return None;
        }
        Some(path)
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
        permit: Arc<OwnedSemaphorePermit>,
    ) -> ListingResult<Vec<PathItem>> {
        let search = search.to_owned();
        let search_paths = collect_dir_entries(
            DirectoryWalk {
                work_tasks: self.work_tasks.clone(),
                running: self.running.clone(),
                path: path.to_path_buf(),
                hidden: Arc::new(self.args.hidden.to_vec()),
                serve_path: self.args.serve_path.clone(),
                rooted_fs: self.rooted_fs.clone(),
                max_entries: self.args.max_search_entries,
                max_duration: Duration::from_secs(self.args.request_timeout),
                permit: Some(permit),
            },
            move |_entry, name| Ok(name.to_lowercase().contains(&search)),
        )
        .await?;

        let mut paths = Vec::with_capacity(search_paths.len());
        for entry in search_paths {
            paths.push(pathitem_from_rooted_entry(
                entry,
                path,
                &self.args.serve_path,
            )?);
        }
        Ok(paths)
    }

    pub(super) async fn handle_zip_dir(
        &self,
        path: &Path,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        let zip_permit = match self.zip_slots.clone().try_acquire_owned() {
            Ok(permit) => Arc::new(permit),
            Err(_) => {
                warn!(
                    "ZIP rejected reason=concurrency_limit limit={}",
                    self.args.max_concurrent_zips
                );
                status_error(
                    res,
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many ZIP operations are running",
                );
                return Ok(());
            }
        };
        let filename = match zip_download_name(path, &self.args.serve_path) {
            Ok(filename) => filename,
            Err(error) => {
                respond_listing_error(res, &error);
                return Ok(());
            }
        };
        set_content_disposition(
            res,
            &format!("{filename}.zip"),
            ContentDispositionFallback::Archive,
        )?;
        res.headers_mut()
            .insert("content-type", HeaderValue::from_static("application/zip"));
        res.headers_mut()
            .typed_insert(CacheControl::new().with_private().with_no_store());
        if head_only {
            return Ok(());
        }
        let (archive, archive_size) = match self.build_zip_archive(path, zip_permit.clone()).await {
            Ok(archive) => archive,
            Err(error) => {
                respond_listing_error(res, &error);
                res.headers_mut().remove("content-type");
                res.headers_mut().remove("content-disposition");
                return Ok(());
            }
        };
        res.headers_mut().typed_insert(ContentLength(archive_size));
        let reader_stream = ReaderStream::with_capacity(archive, BUF_SIZE);
        let stream_body = StreamBody::new(
            reader_stream
                .map_ok(Frame::data)
                .map_err(|err| anyhow!("{err}")),
        );
        *res.body_mut() = PermitBody::new(stream_body, zip_permit).boxed();
        Ok(())
    }

    async fn build_zip_archive(
        &self,
        path: &Path,
        permit: Arc<OwnedSemaphorePermit>,
    ) -> ListingResult<(File, u64)> {
        let output = create_private_tempfile().map_err(|error| {
            ListingError::io("zip_tempfile", path, &self.args.serve_path, &error)
        })?;
        let output = File::from_std(output);
        let mut output = BoundedZipWriter::new(
            output,
            self.disk_space.clone(),
            self.args.max_zip_output_size,
            self.args.min_free_space,
        );
        zip_dir(
            &mut output,
            ZipArchiveSource {
                dir: path,
                hidden: &self.args.hidden,
                compression: self.args.compress.to_compression(),
                serve_path: &self.args.serve_path,
                running: self.running.clone(),
                rooted_fs: &self.rooted_fs,
                work_tasks: self.work_tasks.clone(),
                max_entries: self.args.max_zip_entries,
                max_uncompressed_size: self.args.max_zip_uncompressed_size,
                max_duration: Duration::from_secs(self.args.request_timeout),
                permit,
            },
        )
        .await?;
        output.flush().await.map_err(|error| {
            zip_io_listing_error("zip_flush", path, &self.args.serve_path, &error)
        })?;
        let archive_size = output.written();
        let mut output = output.into_inner();
        output
            .seek(SeekFrom::Start(0))
            .await
            .map_err(|error| ListingError::io("zip_rewind", path, &self.args.serve_path, &error))?;
        Ok((output, archive_size))
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
                &self.args.serve_path,
                "directory_page_path",
                &self.args.serve_path,
            )?
        );
        let data = IndexData {
            href,
            uri_prefix: self.args.uri_prefix.clone(),
            dir_exists: options.exist,
            user: options.session.user,
            csrf_token: options.session.csrf_token,
        };
        let index_data = STANDARD.encode(serde_json::to_string(&data)?);
        let output = INDEX_HTML
            .replace(
                "__ASSETS_PREFIX__",
                &format!("{}{}", self.args.uri_prefix, self.assets_prefix),
            )
            .replace("__INDEX_DATA__", &index_data);
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.body_mut() = body_full(output);
        Ok(())
    }
}

struct ListPageOptions<'a> {
    serve_path: &'a Path,
    hidden: &'a [String],
    last: Option<&'a PathItem>,
    limit: usize,
    sort: &'a str,
    order: &'a str,
    running: &'a AtomicBool,
    max_duration: Duration,
}

fn collect_list_page_blocking(
    rooted_fs: &RootedFs,
    path: &Path,
    options: ListPageOptions<'_>,
) -> ListingResult<Vec<PathItem>> {
    let ListPageOptions {
        serve_path,
        hidden,
        last,
        limit,
        sort,
        order,
        running,
        max_duration,
    } = options;
    let mut selected = Vec::with_capacity(limit.saturating_add(1));
    let started = Instant::now();
    rooted_fs
        .visit_dir_blocking(path, |entry| {
            if !running.load(atomic::Ordering::SeqCst) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "server is stopping",
                ));
            }
            if started.elapsed() > max_duration {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "directory listing exceeded its time budget",
                ));
            }
            let base_name = entry
                .file_name
                .to_str()
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        UNSUPPORTED_FILENAME_MESSAGE,
                    )
                })?
                .to_string();
            let item = pathitem_from_rooted_entry(entry, path, serve_path)
                .map_err(std::io::Error::other)?;
            if is_hidden(hidden, &base_name, item.is_dir())
                || last.is_some_and(|last| {
                    compare_path_items(&item, last, sort, order) != Ordering::Greater
                })
            {
                return Ok(true);
            }
            let index = selected
                .binary_search_by(|existing| compare_path_items(existing, &item, sort, order))
                .unwrap_or_else(|index| index);
            selected.insert(index, item);
            if selected.len() > limit.saturating_add(1) {
                selected.pop();
            }
            Ok(true)
        })
        .map_err(|error| ListingError::io("list_page", path, serve_path, &error))?;
    Ok(selected)
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
    uri_prefix: String,
    dir_exists: bool,
    user: String,
    csrf_token: String,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    paths: Vec<PathItem>,
    next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
            StatusCode::CONFLICT,
            DIRECTORY_CHANGED_DURING_WALK_MESSAGE,
        ));
    }
    Ok(DirectorySnapshot::from_metadata(&metadata))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ListCursor {
    version: u8,
    directory: DirectorySnapshot,
    sort: String,
    order: String,
    query: String,
    last: CursorItem,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorItem {
    path_type: PathType,
    name: String,
    mtime: u64,
    size: u64,
}

impl CursorItem {
    fn to_path_item(&self) -> PathItem {
        PathItem {
            path_type: self.path_type,
            sort_name: self.name.to_lowercase(),
            name: self.name.clone(),
            mtime: self.mtime,
            size: self.size,
        }
    }
}

impl From<&PathItem> for CursorItem {
    fn from(item: &PathItem) -> Self {
        Self {
            path_type: item.path_type,
            name: item.name.clone(),
            mtime: item.mtime,
            size: item.size,
        }
    }
}

fn encode_list_cursor(cursor: &ListCursor) -> String {
    URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(cursor).expect("serializing a list cursor cannot fail"))
}

fn decode_list_cursor(value: &str) -> Result<ListCursor> {
    if value.len() > 4096 {
        return Err(anyhow!("list cursor is too long"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| anyhow!("invalid list cursor encoding"))?;
    let cursor: ListCursor =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid list cursor data"))?;
    if cursor.version != 1 {
        return Err(anyhow!("unsupported list cursor version"));
    }
    Ok(cursor)
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
struct PathItem {
    path_type: PathType,
    #[serde(skip)]
    sort_name: String,
    name: String,
    mtime: u64,
    size: u64,
}

impl PathItem {
    fn is_dir(&self) -> bool {
        self.path_type == PathType::Dir || self.path_type == PathType::SymlinkDir
    }

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

fn respond_listing_error(res: &mut Response, error: &ListingError) {
    error!("{error}");
    status_error(res, error.status, error.public_message);
}

fn zip_io_listing_error(
    operation: &'static str,
    path: &Path,
    serve_root: &Path,
    error: &io::Error,
) -> ListingError {
    match error
        .get_ref()
        .and_then(|source| source.downcast_ref::<ZipWriteFailure>())
    {
        Some(ZipWriteFailure::OutputLimit) => ListingError::limit(
            operation,
            path,
            serve_root,
            "zip_output_size_limit",
            StatusCode::PAYLOAD_TOO_LARGE,
            "ZIP output exceeds the configured size limit",
        ),
        Some(ZipWriteFailure::InsufficientStorage) | None
            if matches!(
                error.kind(),
                io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
            ) =>
        {
            ListingError::limit(
                operation,
                path,
                serve_root,
                "zip_output_storage_limit",
                StatusCode::INSUFFICIENT_STORAGE,
                "Insufficient disk space for ZIP output",
            )
        }
        Some(ZipWriteFailure::InsufficientStorage) => ListingError::limit(
            operation,
            path,
            serve_root,
            "zip_output_storage_limit",
            StatusCode::INSUFFICIENT_STORAGE,
            "Insufficient disk space for ZIP output",
        ),
        None => ListingError::io(operation, path, serve_root, error),
    }
}

fn zip_archive_listing_error(
    operation: &'static str,
    path: &Path,
    serve_root: &Path,
    error: ZipError,
) -> ListingError {
    match error {
        ZipError::Io(error) => zip_io_listing_error(operation, path, serve_root, &error),
        _ => ListingError::archive(operation, path, serve_root),
    }
}

fn strict_file_name<'a>(
    path: &'a Path,
    operation: &'static str,
    serve_root: &Path,
) -> ListingResult<&'a str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ListingError::unsupported_name(operation, path, serve_root))
}

fn zip_download_name(path: &Path, serve_root: &Path) -> ListingResult<String> {
    if path == Path::new("/") {
        return Ok("archive".to_string());
    }
    strict_file_name(path, "zip_filename", serve_root).map(ToOwned::to_owned)
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

fn is_hidden(hidden: &[String], file_name: &str, is_dir: bool) -> bool {
    is_upload_temp_name(file_name)
        || hidden.iter().any(|value| {
            if is_dir && let Some(pattern) = value.strip_suffix('/') {
                return glob(pattern, file_name);
            }
            glob(value, file_name)
        })
}

struct ZipArchiveSource<'a> {
    dir: &'a Path,
    hidden: &'a [String],
    compression: Compression,
    serve_path: &'a Path,
    running: Arc<AtomicBool>,
    rooted_fs: &'a RootedFs,
    work_tasks: TaskTracker,
    max_entries: usize,
    max_uncompressed_size: u64,
    max_duration: Duration,
    permit: Arc<OwnedSemaphorePermit>,
}

async fn zip_dir<W: AsyncWrite + Unpin>(
    writer: &mut W,
    source: ZipArchiveSource<'_>,
) -> ListingResult<()> {
    let ZipArchiveSource {
        dir,
        hidden,
        compression,
        serve_path,
        running,
        rooted_fs,
        work_tasks,
        max_entries,
        max_uncompressed_size,
        max_duration,
        permit,
    } = source;
    let hidden = Arc::new(hidden.to_vec());
    let zip_paths = collect_dir_entries(
        DirectoryWalk {
            work_tasks,
            running,
            path: dir.to_path_buf(),
            hidden,
            serve_path: serve_path.to_path_buf(),
            rooted_fs: rooted_fs.clone(),
            max_entries,
            max_duration,
            permit: Some(permit),
        },
        move |entry, _name| Ok(entry.metadata.is_file()),
    )
    .await?;
    let mut uncompressed_size = 0_u64;
    let mut zip = ZipWriter::new(&mut *writer).with_level(compression);
    for zip_entry in zip_paths {
        let zip_path = zip_entry.path;
        let filename = strict_relative_path(&zip_path, dir, "zip_entry_name", serve_path)?;
        let mut file = rooted_fs
            .open_read(&zip_path)
            .await
            .map_err(|error| ListingError::io("zip_open", &zip_path, serve_path, &error))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|error| ListingError::io("zip_metadata", &zip_path, serve_path, &error))?;
        let options = writer_options(&metadata);
        let mut entry = zip.append_file(&filename, options).await.map_err(|error| {
            zip_archive_listing_error("zip_append", &zip_path, serve_path, error)
        })?;
        copy_zip_source(
            &mut file,
            &mut entry,
            &mut uncompressed_size,
            max_uncompressed_size,
            &zip_path,
            serve_path,
        )
        .await?;
        entry.close().await.map_err(|error| {
            zip_archive_listing_error("zip_entry_finalize", &zip_path, serve_path, error)
        })?;
    }
    zip.finalize()
        .await
        .map_err(|error| zip_archive_listing_error("zip_finalize", dir, serve_path, error))?;
    Ok(())
}

async fn copy_zip_source<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    copied: &mut u64,
    maximum: u64,
    path: &Path,
    serve_root: &Path,
) -> ListingResult<()> {
    let mut buffer = [0_u8; BUF_SIZE];
    loop {
        let remaining = maximum.saturating_sub(*copied);
        let read_limit = if remaining == 0 {
            1
        } else {
            usize::try_from(remaining.min(BUF_SIZE as u64))
                .expect("a ZIP read bounded by BUF_SIZE must fit in usize")
        };
        let bytes_read = reader
            .read(&mut buffer[..read_limit])
            .await
            .map_err(|error| ListingError::io("zip_read", path, serve_root, &error))?;
        if bytes_read == 0 {
            return Ok(());
        }
        if remaining == 0 {
            return Err(ListingError::limit(
                "zip_source_size_limit",
                path,
                serve_root,
                "uncompressed_size_limit",
                StatusCode::PAYLOAD_TOO_LARGE,
                "ZIP source is too large",
            ));
        }
        writer
            .write_all(&buffer[..bytes_read])
            .await
            .map_err(|error| zip_io_listing_error("zip_write", path, serve_root, &error))?;
        *copied = copied
            .checked_add(bytes_read as u64)
            .ok_or_else(|| ListingError::invariant("zip_source_size_overflow", path, serve_root))?;
    }
}

fn writer_options(metadata: &Metadata) -> WriterOptions {
    WriterOptions {
        mtime: metadata.modified().unwrap_or_else(|_| SystemTime::now()),
        permissions: Some(metadata.permissions().mode() & 0o7777),
        uid_gid: Some((metadata.uid(), metadata.gid())),
        comment: None,
    }
}

fn create_private_tempfile() -> std::io::Result<std::fs::File> {
    let file = tempfile::tempfile()?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

struct DirectoryWalk {
    work_tasks: TaskTracker,
    running: Arc<AtomicBool>,
    path: PathBuf,
    hidden: Arc<Vec<String>>,
    serve_path: PathBuf,
    rooted_fs: RootedFs,
    max_entries: usize,
    max_duration: Duration,
    permit: Option<Arc<OwnedSemaphorePermit>>,
}

fn spawn_directory_blocking<F, T>(
    work_tasks: &TaskTracker,
    permit: Option<Arc<OwnedSemaphorePermit>>,
    task: F,
) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    work_tasks.spawn_blocking(move || {
        let _permit = permit;
        task()
    })
}

async fn collect_dir_entries<F>(
    walk: DirectoryWalk,
    include_entry: F,
) -> ListingResult<Vec<RootedDirEntry>>
where
    F: Fn(&RootedDirEntry, &str) -> ListingResult<bool> + Send + 'static,
{
    let DirectoryWalk {
        work_tasks,
        running,
        path,
        hidden,
        serve_path,
        rooted_fs,
        max_entries,
        max_duration,
        permit,
    } = walk;
    let (sender, mut receiver) = mpsc::channel::<ListingResult<RootedDirEntry>>(64);
    let walk_path = path.clone();
    let walk_root = serve_path.clone();
    spawn_directory_blocking(&work_tasks, permit, move || {
        let started = Instant::now();
        let root_metadata = match rooted_fs.metadata_blocking(&walk_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = sender.blocking_send(Err(ListingError::io(
                    "walk_root",
                    &walk_path,
                    &walk_root,
                    &error,
                )));
                return;
            }
        };
        let root_identity = (root_metadata.dev(), root_metadata.ino());
        let mut stack = vec![(walk_path.clone(), HashSet::from([root_identity]))];
        let mut visited = 0usize;

        while let Some((directory, ancestors)) = stack.pop() {
            if !running.load(atomic::Ordering::SeqCst) {
                let _ = sender.blocking_send(Err(ListingError::cancelled(
                    "walk_cancelled",
                    &directory,
                    &walk_root,
                )));
                return;
            }
            if started.elapsed() >= max_duration {
                let _ = sender.blocking_send(Err(ListingError::limit(
                    "walk_timeout",
                    &directory,
                    &walk_root,
                    "time_budget",
                    StatusCode::GATEWAY_TIMEOUT,
                    "Directory operation timed out",
                )));
                return;
            }
            let stopped = std::cell::Cell::new(false);
            let visit_result = rooted_fs.visit_dir_blocking_bounded(
                &directory,
                |entry_path| {
                    if !running.load(atomic::Ordering::SeqCst) {
                        stopped.set(true);
                        let _ = sender.blocking_send(Err(ListingError::cancelled(
                            "walk_cancelled",
                            entry_path,
                            &walk_root,
                        )));
                        return false;
                    }
                    if started.elapsed() >= max_duration {
                        stopped.set(true);
                        let _ = sender.blocking_send(Err(ListingError::limit(
                            "walk_timeout",
                            entry_path,
                            &walk_root,
                            "time_budget",
                            StatusCode::GATEWAY_TIMEOUT,
                            "Directory operation timed out",
                        )));
                        return false;
                    }
                    visited = visited.saturating_add(1);
                    if visited > max_entries {
                        stopped.set(true);
                        let _ = sender.blocking_send(Err(ListingError::limit(
                            "walk_limit",
                            entry_path,
                            &walk_root,
                            "entry_budget",
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "Directory operation exceeded its entry limit",
                        )));
                        return false;
                    }
                    true
                },
                |entry| {
                    // Metadata resolution can itself be slow. Recheck the
                    // cancellation and deadline before using the resolved
                    // entry or descending into it.
                    if !running.load(atomic::Ordering::SeqCst) {
                        stopped.set(true);
                        let _ = sender.blocking_send(Err(ListingError::cancelled(
                            "walk_cancelled",
                            &entry.path,
                            &walk_root,
                        )));
                        return Ok(false);
                    }
                    if started.elapsed() >= max_duration {
                        stopped.set(true);
                        let _ = sender.blocking_send(Err(ListingError::limit(
                            "walk_timeout",
                            &entry.path,
                            &walk_root,
                            "time_budget",
                            StatusCode::GATEWAY_TIMEOUT,
                            "Directory operation timed out",
                        )));
                        return Ok(false);
                    }
                    let Some(base_name) = entry.file_name.to_str() else {
                        stopped.set(true);
                        let _ = sender.blocking_send(Err(ListingError::unsupported_name(
                            "walk_filename",
                            &entry.path,
                            &walk_root,
                        )));
                        return Ok(false);
                    };
                    let is_dir = entry.metadata.is_dir();
                    if is_hidden(&hidden, base_name, is_dir) {
                        return Ok(true);
                    }
                    let include = match include_entry(&entry, base_name) {
                        Ok(include) => include,
                        Err(error) => {
                            stopped.set(true);
                            let _ = sender.blocking_send(Err(error));
                            return Ok(false);
                        }
                    };
                    let child = if is_dir {
                        let identity = (entry.metadata.dev(), entry.metadata.ino());
                        if ancestors.contains(&identity) {
                            stopped.set(true);
                            let error = std::io::Error::from_raw_os_error(40);
                            let _ = sender.blocking_send(Err(ListingError::io(
                                "walk_loop",
                                &entry.path,
                                &walk_root,
                                &error,
                            )));
                            return Ok(false);
                        }
                        let mut child_ancestors = ancestors.clone();
                        child_ancestors.insert(identity);
                        Some((entry.path.clone(), child_ancestors))
                    } else {
                        None
                    };
                    if include && sender.blocking_send(Ok(entry)).is_err() {
                        stopped.set(true);
                        return Ok(false);
                    }
                    if let Some(child) = child {
                        stack.push(child);
                    }
                    Ok(true)
                },
            );
            if stopped.get() {
                return;
            }
            if let Err(error) = visit_result {
                let _ = sender.blocking_send(Err(ListingError::io(
                    "walk_next",
                    &directory,
                    &walk_root,
                    &error,
                )));
                return;
            }
        }
    });

    let mut paths = Vec::new();
    while let Some(result) = receiver.recv().await {
        paths.push(result?);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;
    use std::{
        ffi::OsString,
        os::{fd::BorrowedFd, unix::ffi::OsStringExt},
        sync::{Condvar, Mutex as StdMutex},
    };
    use tokio_util::sync::CancellationToken;

    struct AlwaysPendingFile {
        inner: File,
    }

    impl AsFd for AlwaysPendingFile {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.inner.as_fd()
        }
    }

    impl AsyncWrite for AlwaysPendingFile {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn non_utf8_and_control_bytes_are_safely_escaped_in_errors() {
        let root = Path::new("/srv/share");
        let name = OsString::from_vec(b"line\nquote\"slash\\bad\xff".to_vec());
        let path = root.join(name);
        let error = ListingError::unsupported_name("list_filename", &path, root);
        let rendered = error.to_string();

        assert_eq!(error.relative_path, "line\\x0aquote\\\"slash\\\\bad\\xff");
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains("/srv/share"));
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.public_message, UNSUPPORTED_FILENAME_MESSAGE);
    }

    #[test]
    fn disappearing_walk_entries_are_reported_as_retryable_conflicts() {
        let root = Path::new("/srv/share");
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::NotADirectory,
        ] {
            let io_error = std::io::Error::new(kind, "entry changed concurrently");
            let error = ListingError::io("walk_next", root, root, &io_error);

            assert_eq!(error.status, StatusCode::CONFLICT);
            assert_eq!(error.public_message, DIRECTORY_CHANGED_DURING_WALK_MESSAGE);
            assert!(error.reason.contains(&format!("io_kind={kind:?}")));
        }
    }

    #[test]
    fn post_walk_snapshot_disappearance_or_type_change_is_a_retryable_conflict() {
        let root = assert_fs::TempDir::new().expect("create listing root");
        let file = root.path().join("replacement.txt");
        std::fs::write(&file, "content").expect("create non-directory replacement");

        for error in [
            directory_snapshot_after_walk(
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "directory disappeared",
                )),
                root.path(),
                root.path(),
            )
            .expect_err("disappeared directory must conflict"),
            directory_snapshot_after_walk(
                Ok(std::fs::metadata(&file).expect("read replacement metadata")),
                root.path(),
                root.path(),
            )
            .expect_err("non-directory replacement must conflict"),
        ] {
            assert_eq!(error.status, StatusCode::CONFLICT);
            assert_eq!(error.public_message, DIRECTORY_CHANGED_DURING_WALK_MESSAGE);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_search_and_zip_worker_keeps_permit_after_waiter_timeout() {
        let work_tasks = TaskTracker::new();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::new(
            slots
                .clone()
                .try_acquire_owned()
                .expect("acquire directory slot"),
        );
        let gate = Arc::new((StdMutex::new(false), Condvar::new()));
        let worker_gate = gate.clone();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let worker = spawn_directory_blocking(&work_tasks, Some(permit.clone()), move || {
            let _ = started_sender.send(());
            let (released, condition) = &*worker_gate;
            let mut released = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = condition
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        });
        drop(permit);
        started_receiver.await.expect("worker must start");

        assert!(
            tokio::time::timeout(Duration::ZERO, worker).await.is_err(),
            "the request-side waiter should time out"
        );
        assert_eq!(slots.available_permits(), 0);

        let (released, condition) = &*gate;
        *released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        condition.notify_one();
        work_tasks.close();
        work_tasks.wait().await;
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn missing_walk_root_is_a_retryable_conflict() {
        let root = assert_fs::TempDir::new().expect("create rooted filesystem");
        let missing = root.path().join("removed-before-walk");
        std::fs::create_dir(&missing).expect("create directory to remove");
        let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
        std::fs::remove_dir(&missing).expect("remove directory before walk");

        let error = collect_dir_entries(
            DirectoryWalk {
                work_tasks: TaskTracker::new(),
                running: Arc::new(AtomicBool::new(true)),
                path: missing,
                hidden: Arc::new(Vec::new()),
                serve_path: root.path().to_path_buf(),
                rooted_fs,
                max_entries: 16,
                max_duration: Duration::from_secs(5),
                permit: None,
            },
            |_, _| Ok(true),
        )
        .await
        .expect_err("a disappeared walk root must not produce a partial result");

        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.public_message, DIRECTORY_CHANGED_DURING_WALK_MESSAGE);
    }

    #[tokio::test]
    async fn nested_directory_disappearance_is_a_retryable_conflict() {
        let root = assert_fs::TempDir::new().expect("create rooted filesystem");
        let walk_root = root.path().join("walk");
        let disappearing = walk_root.join("disappearing");
        std::fs::create_dir_all(&disappearing).expect("create nested directory");
        std::fs::write(walk_root.join("possible-partial-result.txt"), "content")
            .expect("create possible partial result");
        let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
        let remove_path = disappearing.clone();

        let error = collect_dir_entries(
            DirectoryWalk {
                work_tasks: TaskTracker::new(),
                running: Arc::new(AtomicBool::new(true)),
                path: walk_root,
                hidden: Arc::new(Vec::new()),
                serve_path: root.path().to_path_buf(),
                rooted_fs,
                max_entries: 16,
                max_duration: Duration::from_secs(5),
                permit: None,
            },
            move |entry, _| {
                if entry.path == remove_path {
                    std::fs::remove_dir(&remove_path)
                        .expect("remove nested directory during traversal");
                }
                Ok(entry.metadata.is_file())
            },
        )
        .await
        .expect_err("a disappeared nested directory must discard partial results");

        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.public_message, DIRECTORY_CHANGED_DURING_WALK_MESSAGE);
    }

    #[test]
    fn zip_tempfile_is_private() {
        let file = create_private_tempfile().expect("create ZIP tempfile");
        let mode = file
            .metadata()
            .expect("read ZIP tempfile metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn pending_zip_write_keeps_one_reservation_until_the_writer_is_dropped() {
        let tracker = DiskSpaceTracker::new();
        let output = AlwaysPendingFile {
            inner: File::from_std(create_private_tempfile().expect("create ZIP tempfile")),
        };
        let mut writer = BoundedZipWriter::new(output, tracker.clone(), 1024, 0);
        let waker = futures_util::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);

        assert!(
            Pin::new(&mut writer)
                .poll_write(&mut context, b"abc")
                .is_pending()
        );
        assert_eq!(tracker.total_reserved_for_tests(), 3);
        assert!(
            Pin::new(&mut writer)
                .poll_write(&mut context, b"abc")
                .is_pending()
        );
        assert_eq!(tracker.total_reserved_for_tests(), 3);

        drop(writer);
        assert_eq!(tracker.total_reserved_for_tests(), 0);
    }

    #[tokio::test]
    async fn zip_output_writer_never_writes_beyond_its_hard_limit() {
        let output = File::from_std(create_private_tempfile().expect("create ZIP tempfile"));
        let mut writer = BoundedZipWriter::new(output, DiskSpaceTracker::new(), 3, 0);

        writer.write_all(b"abc").await.expect("write within limit");
        let error = writer
            .write_all(b"d")
            .await
            .expect_err("write beyond ZIP output limit must fail");

        assert_eq!(writer.written(), 3);
        assert_eq!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<ZipWriteFailure>()),
            Some(&ZipWriteFailure::OutputLimit)
        );
    }

    #[tokio::test]
    async fn zip_source_limit_uses_bytes_read_even_if_metadata_was_smaller() {
        let mut growing_source = &b"abcdef"[..];
        let mut archive_entry = Vec::new();
        let mut copied = 0;

        let error = copy_zip_source(
            &mut growing_source,
            &mut archive_entry,
            &mut copied,
            4,
            Path::new("/source"),
            Path::new("/"),
        )
        .await
        .expect_err("source growth beyond the configured limit must fail");

        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.public_message, "ZIP source is too large");
        assert_eq!(copied, 4);
        assert_eq!(archive_entry, b"abcd");
    }

    #[tokio::test]
    async fn zip_output_and_free_space_limits_have_clear_http_statuses() {
        let root = assert_fs::TempDir::new().expect("create temporary ZIP root");
        std::fs::write(root.path().join("file.txt"), "content").expect("create ZIP source");

        for (max_output_size, minimum_free, expected_status) in [
            (1, 0, StatusCode::PAYLOAD_TOO_LARGE),
            (1024 * 1024, u64::MAX, StatusCode::INSUFFICIENT_STORAGE),
        ] {
            let server = Server::init(
                Args {
                    serve_path: root.path().to_path_buf(),
                    max_zip_output_size: max_output_size,
                    min_free_space: minimum_free,
                    ..Args::default()
                },
                Arc::new(AtomicBool::new(true)),
                TaskTracker::new(),
                TaskTracker::new(),
                CancellationToken::new(),
                CancellationToken::new(),
            )
            .expect("construct test server");
            let mut response = Response::default();

            server
                .handle_zip_dir(root.path(), false, &mut response)
                .await
                .expect("handle ZIP request");

            assert_eq!(response.status(), expected_status);
            assert!(!response.headers().contains_key("content-disposition"));
            assert!(!response.headers().contains_key("content-type"));
        }
    }

    #[test]
    fn filesystem_root_has_a_safe_zip_download_name() {
        assert_eq!(
            zip_download_name(Path::new("/"), Path::new("/")).expect("root ZIP name"),
            "archive"
        );
    }

    #[tokio::test]
    async fn zip_permit_is_held_until_the_response_body_finishes_or_is_dropped() {
        let root = assert_fs::TempDir::new().expect("create temporary ZIP root");
        std::fs::write(root.path().join("file.txt"), "content").expect("create ZIP source");
        let server = Server::init(
            Args {
                serve_path: root.path().to_path_buf(),
                max_concurrent_zips: 1,
                min_free_space: 0,
                ..Args::default()
            },
            Arc::new(AtomicBool::new(true)),
            TaskTracker::new(),
            TaskTracker::new(),
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .expect("construct test server");

        let mut first = Response::default();
        server
            .handle_zip_dir(root.path(), false, &mut first)
            .await
            .expect("build first ZIP response");
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(server.zip_slots.available_permits(), 0);

        let mut rejected = Response::default();
        server
            .handle_zip_dir(root.path(), false, &mut rejected)
            .await
            .expect("reject concurrent ZIP response");
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(server.zip_slots.available_permits(), 0);

        first
            .into_body()
            .collect()
            .await
            .expect("consume first ZIP response");
        assert_eq!(server.zip_slots.available_permits(), 1);

        let mut cancelled = Response::default();
        server
            .handle_zip_dir(root.path(), false, &mut cancelled)
            .await
            .expect("build ZIP response to cancel");
        assert_eq!(cancelled.status(), StatusCode::OK);
        assert_eq!(server.zip_slots.available_permits(), 0);
        drop(cancelled);
        assert_eq!(server.zip_slots.available_permits(), 1);
    }
}
