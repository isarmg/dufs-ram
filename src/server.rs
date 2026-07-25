mod browser_api;
mod disk_space;
mod download;
mod listing;
mod path_coordinator;
mod rooted_fs;
mod session;
mod storage;
mod upload;

use self::{
    browser_api::BROWSER_API_PREFIX,
    disk_space::DiskSpaceTracker,
    listing::LIST_API_PATH,
    path_coordinator::PathCoordinator,
    rooted_fs::{RootedEntryKey, RootedFs},
    session::{LOGIN_ERROR_QUERY, LOGIN_PATH, LOGOUT_PATH, LoginErrorStore},
    storage::DurableStorage,
    upload::{
        UploadOptions, is_upload_temp_name, parse_upload_id, parse_upload_length,
        parse_upload_offset,
    },
};
use crate::{
    Args, app_error::AppError, auth::session_token_from_cookie, http_utils::body_full,
    request_context::RequestContext, utils::decode_uri,
};

use anyhow::Result;
use bytes::Bytes;
use headers::{ContentType, HeaderMapExt};
use http_body_util::combinators::BoxBody;
use hyper::{
    Method, StatusCode,
    body::Incoming,
    header::{CACHE_CONTROL, CONTENT_DISPOSITION, COOKIE, HeaderValue},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, SystemTime},
};
use tokio::sync::Semaphore;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub type Request = hyper::Request<Incoming>;
pub type Response = hyper::Response<BoxBody<Bytes, anyhow::Error>>;

const INDEX_CSS: &str = include_str!("../assets/index.css");
const INDEX_JS: &str = include_str!("../assets/index.js");
const MODULE_API_JS: &str = include_str!("../assets/modules/api.js");
const MODULE_APP_JS: &str = include_str!("../assets/modules/app.js");
const MODULE_DOM_JS: &str = include_str!("../assets/modules/dom.js");
const MODULE_LISTING_JS: &str = include_str!("../assets/modules/listing.js");
const MODULE_OPERATIONS_JS: &str = include_str!("../assets/modules/operations.js");
const MODULE_PATH_JS: &str = include_str!("../assets/modules/path.js");
const MODULE_UPLOAD_JS: &str = include_str!("../assets/modules/upload.js");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");
const BUF_SIZE: usize = 65536;
const HEALTH_CHECK_PATH: &str = "__dufs__/health";
const AUTH_ERROR_HEADER: &str = "x-dufs-auth-error";
const CSRF_AUTH_ERROR: &str = "csrf";

pub struct Server {
    args: Args,
    assets_prefix: String,
    running: Arc<AtomicBool>,
    path_coordinator: PathCoordinator,
    rooted_fs: RootedFs,
    storage: DurableStorage,
    active_upload_files: Arc<Mutex<HashSet<RootedEntryKey>>>,
    login_slots: Arc<Semaphore>,
    upload_slots: Arc<Semaphore>,
    search_slots: Arc<Semaphore>,
    zip_slots: Arc<Semaphore>,
    disk_space: DiskSpaceTracker,
    login_errors: Mutex<LoginErrorStore>,
    work_tasks: TaskTracker,
    commit_tasks: TaskTracker,
    shutdown: CancellationToken,
    force_shutdown: CancellationToken,
}

impl Server {
    pub fn init(
        args: Args,
        running: Arc<AtomicBool>,
        work_tasks: TaskTracker,
        commit_tasks: TaskTracker,
        shutdown: CancellationToken,
        force_shutdown: CancellationToken,
    ) -> Result<Self> {
        let assets_prefix = embedded_assets_prefix();
        let rooted_fs = RootedFs::new(&args.serve_path)?;
        let max_concurrent_uploads = args.max_concurrent_uploads;
        let max_concurrent_searches = args.max_concurrent_searches;
        let max_concurrent_zips = args.max_concurrent_zips;
        let storage = DurableStorage::new(rooted_fs.clone());
        Ok(Self {
            args,
            running,
            assets_prefix,
            path_coordinator: PathCoordinator::new(rooted_fs.clone()),
            rooted_fs,
            storage,
            active_upload_files: Arc::new(Mutex::new(HashSet::new())),
            login_slots: Arc::new(Semaphore::new(2)),
            upload_slots: Arc::new(Semaphore::new(max_concurrent_uploads)),
            search_slots: Arc::new(Semaphore::new(max_concurrent_searches)),
            zip_slots: Arc::new(Semaphore::new(max_concurrent_zips)),
            disk_space: DiskSpaceTracker::new(),
            login_errors: Mutex::new(LoginErrorStore::default()),
            work_tasks,
            commit_tasks,
            shutdown,
            force_shutdown,
        })
    }

    pub(super) fn spawn_background<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let force_shutdown = self.force_shutdown.clone();
        drop(self.work_tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = force_shutdown.cancelled() => {}
                _ = task => {}
            }
        }));
    }

    pub fn start_maintenance(self: &Arc<Self>) {
        let server = self.clone();
        drop(self.work_tasks.spawn(async move {
            server.run_upload_maintenance().await;
        }));
    }

    pub(super) async fn run_commit<F, T>(&self, task: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        self.commit_tasks
            .spawn(async move {
                let result = task.await;
                if let Err(err) = &result {
                    error!("Tracked filesystem mutation failed error={err:#}");
                }
                result
            })
            .await?
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
        self.run_commit(async move {
            let _upload_permit = upload_permit;
            let mut response = Response::default();
            server
                .handle_upload(&path, options, req, &mut response)
                .await?;
            Ok(response)
        })
        .await
    }

    pub async fn call(
        self: Arc<Self>,
        req: Request,
        addr: SocketAddr,
    ) -> Result<Response, hyper::Error> {
        let public_asset_request = req.method() == Method::GET
            && self
                .resolve_path(req.uri().path())
                .as_deref()
                .is_some_and(|path| self.is_public_asset_path(path));
        let upload_request = matches!(req.method(), &Method::PUT | &Method::PATCH);
        let mut context = RequestContext::new(&req, addr, &self.args.http_logger);
        let handle = self.clone().handle(req, &mut context);
        let handle_result = if upload_request {
            handle.await
        } else {
            match tokio::time::timeout(Duration::from_secs(self.args.request_timeout), handle).await
            {
                Ok(result) => result,
                Err(_) => {
                    let mut res = Response::default();
                    status_error(&mut res, StatusCode::GATEWAY_TIMEOUT, "Request timed out");
                    context.access_log_mut().insert(
                        "status".to_string(),
                        StatusCode::GATEWAY_TIMEOUT.as_u16().to_string(),
                    );
                    self.args.http_logger.log(
                        context.access_log(),
                        Some("request time budget exceeded".to_string()),
                    );
                    set_private_no_store(&mut res);
                    return Ok(res);
                }
            }
        };

        let (mut res, successful_public_asset) = match handle_result {
            Ok(res) => {
                let successful_public_asset =
                    public_asset_request && res.status() == StatusCode::OK;
                context
                    .access_log_mut()
                    .insert("status".to_string(), res.status().as_u16().to_string());
                if !successful_public_asset {
                    self.args.http_logger.log(context.access_log(), None);
                }
                (res, successful_public_asset)
            }
            Err(err) => {
                let mut res = Response::default();
                let error = AppError::internal(err);
                apply_app_error(&mut res, &error);
                context
                    .access_log_mut()
                    .insert("status".to_string(), error.status().as_u16().to_string());
                self.args
                    .http_logger
                    .log(context.access_log(), Some(error.to_string()));
                (res, false)
            }
        };
        if !successful_public_asset {
            set_private_no_store(&mut res);
        }

        Ok(res)
    }

    pub async fn handle(
        self: Arc<Self>,
        req: Request,
        context: &mut RequestContext,
    ) -> Result<Response> {
        let mut res = Response::default();

        let req_path = req.uri().path();
        let headers = req.headers();
        let method = req.method().clone();

        let relative_path = match self.resolve_path(req_path) {
            Some(value) => value,
            None => {
                status_bad_request(&mut res, "Invalid Path");
                return Ok(res);
            }
        };

        let query = req.uri().query().unwrap_or_default();
        let query_params: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();

        if relative_path == LOGIN_PATH {
            match method {
                Method::GET => {
                    self.send_login_page_for_get(
                        query_params.get(LOGIN_ERROR_QUERY).map(String::as_str),
                        &mut res,
                    )?;
                }
                Method::POST => {
                    if let Some(user) = self.handle_login(req, &mut res).await? {
                        self.args
                            .http_logger
                            .set_authenticated_user(context.access_log_mut(), &user);
                    }
                }
                _ => {
                    *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                }
            }
            return Ok(res);
        }

        let session_token = headers
            .get(COOKIE)
            .and_then(session_token_from_cookie)
            .map(str::to_owned);
        let Some((session_token, session)) = session_token.and_then(|token| {
            self.args
                .auth
                .authenticate(&token)
                .map(|session| (token, session))
        }) else {
            self.reject_unauthenticated(&method, headers, &mut res)?;
            return Ok(res);
        };
        self.args
            .http_logger
            .set_authenticated_user(context.access_log_mut(), &session.user);

        if matches!(
            method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        ) && !self.csrf_is_valid(headers, req.uri(), &session_token, &session.csrf_token)
        {
            status_csrf_forbid(&mut res);
            return Ok(res);
        }

        if method == Method::POST && relative_path == LOGOUT_PATH {
            self.handle_logout(&session_token, &mut res);
            return Ok(res);
        }

        if method == Method::GET && self.handle_internal(&relative_path, &mut res) {
            return Ok(res);
        }

        if method == Method::GET && relative_path == LIST_API_PATH {
            self.handle_list_api(&query_params, &mut res).await?;
            return Ok(res);
        }

        if method == Method::POST && relative_path.starts_with(BROWSER_API_PREFIX) {
            self.handle_browser_api(&relative_path, req, &mut res)
                .await?;
            return Ok(res);
        }

        if relative_path
            .split('/')
            .next()
            .is_some_and(|part| self.is_reserved_internal_component(part))
        {
            status_not_found(&mut res);
            return Ok(res);
        }

        let head_only = method == Method::HEAD;

        let path = match self.join_path(&relative_path) {
            Some(value) => value,
            None => {
                status_forbid(&mut res);
                return Ok(res);
            }
        };
        let path = path.as_path();

        if method == Method::DELETE && self.is_managed_root(path) {
            status_forbid(&mut res);
            return Ok(res);
        }

        let mut upload_permit = if matches!(method, Method::PUT | Method::PATCH) {
            match self.upload_slots.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    status_error(
                        &mut res,
                        StatusCode::TOO_MANY_REQUESTS,
                        "Too many concurrent uploads",
                    );
                    res.headers_mut()
                        .insert("retry-after", HeaderValue::from_static("1"));
                    return Ok(res);
                }
            }
        } else {
            None
        };

        let mut path_lease = if matches!(method, Method::PUT | Method::PATCH | Method::DELETE) {
            Some(self.path_coordinator.acquire([path]).await)
        } else {
            None
        };

        // Follow normal root-contained links for reads. If the final component
        // is a dangling link or a link cycle, fall back to metadata for the link
        // itself so authenticated DELETE and PUT can remove or replace it.
        // openat2 reports root escapes as XDEV; those deliberately do not use
        // the no-follow fallback and remain invisible.
        let metadata = match self.rooted_fs.metadata(path).await {
            Ok(metadata) => Some(metadata),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                self.rooted_fs.metadata_nofollow(path).await.ok()
            }
            Err(_) => None,
        };
        let (is_miss, is_dir, is_file) = match metadata {
            Some(meta) => (false, meta.is_dir(), meta.is_file()),
            None => (true, false, false),
        };

        if is_miss && self.guard_root_contained(path).await {
            status_not_found(&mut res);
            return Ok(res);
        }

        if method == Method::HEAD {
            match parse_upload_id(headers) {
                Ok(Some(upload_id)) => {
                    self.handle_upload_status(path, upload_id, &mut res).await?;
                    return Ok(res);
                }
                Ok(None) => {}
                Err(err) => {
                    status_bad_request(&mut res, &err.to_string());
                    return Ok(res);
                }
            }
        }

        match method {
            Method::GET | Method::HEAD => {
                if is_dir {
                    if has_query_flag(&query_params, "zip") {
                        self.handle_zip_dir(path, head_only, &mut res).await?;
                    } else if query_params.contains_key("q") {
                        self.handle_search_dir(path, &query_params, head_only, session, &mut res)
                            .await?;
                    } else {
                        self.handle_ls_dir(path, true, &query_params, head_only, session, &mut res)
                            .await?;
                    }
                } else if is_file {
                    self.handle_send_file(path, headers, head_only, &mut res)
                        .await?;
                } else if req_path.ends_with('/') {
                    self.handle_ls_dir(path, false, &query_params, head_only, session, &mut res)
                        .await?;
                } else {
                    status_not_found(&mut res);
                }
            }
            Method::PUT => {
                if is_dir {
                    status_error(&mut res, StatusCode::CONFLICT, "Target is a directory");
                } else {
                    let upload_id = match parse_upload_id(headers) {
                        Ok(Some(value)) => value,
                        Ok(None) => {
                            status_bad_request(&mut res, "The x-dufs-upload-id header is required");
                            return Ok(res);
                        }
                        Err(err) => {
                            status_bad_request(&mut res, &err.to_string());
                            return Ok(res);
                        }
                    };
                    let upload_length = match parse_upload_length(headers) {
                        Ok(Some(value)) => value,
                        Ok(None) => {
                            status_bad_request(
                                &mut res,
                                "The x-dufs-upload-length header is required",
                            );
                            return Ok(res);
                        }
                        Err(err) => {
                            status_bad_request(&mut res, &err.to_string());
                            return Ok(res);
                        }
                    };
                    res = self
                        .run_tracked_upload(
                            path,
                            UploadOptions {
                                resume: false,
                                upload_id,
                                upload_length,
                                upload_offset: None,
                                path_lease: path_lease.take().expect("PUT acquired a path lease"),
                            },
                            req,
                            upload_permit.take().expect("PUT acquired an upload permit"),
                        )
                        .await?;
                }
            }
            Method::PATCH => {
                let upload_id = match parse_upload_id(headers) {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        status_bad_request(&mut res, "The x-dufs-upload-id header is required");
                        return Ok(res);
                    }
                    Err(err) => {
                        status_bad_request(&mut res, &err.to_string());
                        return Ok(res);
                    }
                };
                let upload_length = match parse_upload_length(headers) {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        status_bad_request(&mut res, "The x-dufs-upload-length header is required");
                        return Ok(res);
                    }
                    Err(err) => {
                        status_bad_request(&mut res, &err.to_string());
                        return Ok(res);
                    }
                };
                let upload_offset = match parse_upload_offset(headers) {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        status_bad_request(&mut res, "The x-dufs-upload-offset header is required");
                        return Ok(res);
                    }
                    Err(err) => {
                        status_bad_request(&mut res, &err.to_string());
                        return Ok(res);
                    }
                };
                res = self
                    .run_tracked_upload(
                        path,
                        UploadOptions {
                            resume: true,
                            upload_id,
                            upload_length,
                            upload_offset: Some(upload_offset),
                            path_lease: path_lease.take().expect("PATCH acquired a path lease"),
                        },
                        req,
                        upload_permit
                            .take()
                            .expect("PATCH acquired an upload permit"),
                    )
                    .await?;
            }
            Method::DELETE => {
                if is_miss {
                    status_not_found(&mut res);
                } else {
                    self.handle_delete(
                        path,
                        &mut res,
                        path_lease.take().expect("DELETE acquired a path lease"),
                    )
                    .await?;
                }
            }
            _ => {
                *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
            }
        }
        Ok(res)
    }

    async fn handle_delete(
        &self,
        path: &Path,
        res: &mut Response,
        path_lease: path_coordinator::PathLease,
    ) -> Result<()> {
        let rooted_fs = self.rooted_fs.clone();
        let path = path.to_path_buf();
        let trash = self
            .run_commit(async move {
                let _path_lease = path_lease;
                Ok(rooted_fs.move_to_trash(&path).await?)
            })
            .await?;
        self.spawn_background(async move {
            if let Err(err) = trash.purge().await {
                warn!("Failed to purge an internal trash entry error={err:#}");
            }
        });
        status_no_content(res);
        Ok(())
    }

    fn handle_internal(&self, req_path: &str, res: &mut Response) -> bool {
        if let Some(name) = req_path.strip_prefix(&self.assets_prefix) {
            match name {
                "index.js"
                | "modules/api.js"
                | "modules/app.js"
                | "modules/dom.js"
                | "modules/listing.js"
                | "modules/operations.js"
                | "modules/path.js"
                | "modules/upload.js" => {
                    let source = match name {
                        "index.js" => INDEX_JS,
                        "modules/api.js" => MODULE_API_JS,
                        "modules/app.js" => MODULE_APP_JS,
                        "modules/dom.js" => MODULE_DOM_JS,
                        "modules/listing.js" => MODULE_LISTING_JS,
                        "modules/operations.js" => MODULE_OPERATIONS_JS,
                        "modules/path.js" => MODULE_PATH_JS,
                        "modules/upload.js" => MODULE_UPLOAD_JS,
                        _ => unreachable!(),
                    };
                    *res.body_mut() = body_full(source);
                    res.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_static("application/javascript; charset=UTF-8"),
                    );
                }
                "index.css" => {
                    *res.body_mut() = body_full(INDEX_CSS);
                    res.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_static("text/css; charset=UTF-8"),
                    );
                }
                "favicon.ico" => {
                    *res.body_mut() = body_full(FAVICON_ICO);
                    res.headers_mut()
                        .insert("content-type", HeaderValue::from_static("image/x-icon"));
                }
                _ => status_not_found(res),
            }
            if res.status() == StatusCode::OK {
                res.headers_mut().insert(
                    CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=31536000, immutable"),
                );
            }
            res.headers_mut().insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            true
        } else if req_path == HEALTH_CHECK_PATH {
            res.headers_mut()
                .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
            *res.body_mut() = body_full(r#"{"status":"OK"}"#);
            true
        } else {
            false
        }
    }

    fn is_public_asset_path(&self, req_path: &str) -> bool {
        req_path
            .strip_prefix(&self.assets_prefix)
            .is_some_and(|name| {
                matches!(
                    name,
                    "index.js"
                        | "index.css"
                        | "favicon.ico"
                        | "modules/api.js"
                        | "modules/app.js"
                        | "modules/dom.js"
                        | "modules/listing.js"
                        | "modules/operations.js"
                        | "modules/path.js"
                        | "modules/upload.js"
                )
            })
    }

    async fn guard_root_contained(&self, path: &Path) -> bool {
        let mut check_path = path.to_path_buf();
        loop {
            match self.rooted_fs.metadata(&check_path).await {
                Ok(_) => return false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    match check_path.parent() {
                        Some(parent) if check_path != self.args.serve_path => {
                            check_path = parent.to_path_buf();
                        }
                        _ => return true,
                    }
                }
                Err(_) => return true,
            }
        }
    }

    fn resolve_path(&self, path: &str) -> Option<String> {
        let path = decode_uri(path)?;
        let path = path.trim_matches('/');
        let mut parts = vec![];
        for component in Path::new(path).components() {
            if let Component::Normal(value) = component {
                let value = value.to_string_lossy();
                if is_upload_temp_name(&value) {
                    return None;
                }
                parts.push(value);
            } else {
                return None;
            }
        }
        let new_path = parts.join("/");
        let path_prefix = self.args.path_prefix.as_str();
        if path_prefix.is_empty() {
            return Some(new_path);
        }
        if new_path == path_prefix {
            return Some(String::new());
        }
        new_path
            .strip_prefix(&format!("{path_prefix}/"))
            .map(|value| value.trim_matches('/').to_string())
    }

    fn join_path(&self, path: &str) -> Option<PathBuf> {
        if path.is_empty() {
            return Some(self.args.serve_path.clone());
        }
        Some(self.args.serve_path.join(path))
    }

    pub(super) fn is_managed_root(&self, path: &Path) -> bool {
        path == self.args.serve_path
    }

    pub(super) fn is_reserved_internal_component(&self, component: &str) -> bool {
        component == "__dufs__" || self.assets_prefix.strip_suffix('/') == Some(component)
    }
}

fn embedded_assets_prefix() -> String {
    let mut digest = Sha256::new();
    for (name, contents) in [
        ("index.js", INDEX_JS.as_bytes()),
        ("index.css", INDEX_CSS.as_bytes()),
        ("favicon.ico", FAVICON_ICO),
        ("modules/api.js", MODULE_API_JS.as_bytes()),
        ("modules/app.js", MODULE_APP_JS.as_bytes()),
        ("modules/dom.js", MODULE_DOM_JS.as_bytes()),
        ("modules/listing.js", MODULE_LISTING_JS.as_bytes()),
        ("modules/operations.js", MODULE_OPERATIONS_JS.as_bytes()),
        ("modules/path.js", MODULE_PATH_JS.as_bytes()),
        ("modules/upload.js", MODULE_UPLOAD_JS.as_bytes()),
    ] {
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((contents.len() as u64).to_be_bytes());
        digest.update(contents);
    }
    format!("__dufs_assets_{}/", hex::encode(digest.finalize()))
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

#[derive(Clone, Copy)]
enum ContentDispositionFallback {
    File,
    Archive,
}

fn set_content_disposition(
    res: &mut Response,
    filename: &str,
    fallback: ContentDispositionFallback,
) -> Result<()> {
    let fallback_filename = match fallback {
        ContentDispositionFallback::File => "download",
        ContentDispositionFallback::Archive => "archive.zip",
    };
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
        "attachment; filename=\"{fallback_filename}\"; filename*=UTF-8''{}",
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

fn has_query_flag(query_params: &HashMap<String, String>, name: &str) -> bool {
    query_params
        .get(name)
        .map(|value| value.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[test]
    fn server_init_rejects_a_non_directory_root() {
        let temp = assert_fs::TempDir::new().unwrap();
        let file = temp.path().join("shared.txt");
        std::fs::write(&file, "contents").unwrap();
        let args = Args {
            serve_path: file,
            ..Args::default()
        };
        let error = match Server::init(
            args,
            Arc::new(AtomicBool::new(true)),
            TaskTracker::new(),
            TaskTracker::new(),
            CancellationToken::new(),
            CancellationToken::new(),
        ) {
            Ok(_) => panic!("a regular file must not be accepted as the shared root"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("not a directory"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn tracked_mutation_keeps_its_path_lease_after_waiter_cancellation() {
        let temp = assert_fs::TempDir::new().unwrap();
        let args = Args {
            serve_path: temp.path().to_path_buf(),
            ..Args::default()
        };
        let work_tasks = TaskTracker::new();
        let commit_tasks = TaskTracker::new();
        let server = Arc::new(
            Server::init(
                args,
                Arc::new(AtomicBool::new(true)),
                work_tasks,
                commit_tasks.clone(),
                CancellationToken::new(),
                CancellationToken::new(),
            )
            .unwrap(),
        );
        let target = temp.path().join("directory/file.txt");
        let ancestor = temp.path().join("directory");
        let path_lease = server.path_coordinator.acquire([&target]).await;
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();

        let waiter = {
            let server = server.clone();
            tokio::spawn(async move {
                server
                    .run_commit(async move {
                        let _path_lease = path_lease;
                        let _ = started_tx.send(());
                        let _ = release_rx.await;
                        Ok(())
                    })
                    .await
            })
        };
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());

        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                server.path_coordinator.acquire([&ancestor]),
            )
            .await
            .is_err(),
            "cancelling the HTTP waiter released a live mutation lease"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            server.path_coordinator.acquire([&ancestor]),
        )
        .await
        .expect("tracked mutation did not release its lease after completion");
        commit_tasks.close();
        tokio::time::timeout(Duration::from_secs(1), commit_tasks.wait())
            .await
            .expect("tracked mutation task did not finish");
    }
}
