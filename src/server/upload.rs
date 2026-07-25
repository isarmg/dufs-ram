use super::{
    Request, Response, Server,
    disk_space::DiskSpaceReservation,
    path_coordinator::PathLease,
    rooted_fs::{RootedEntryKey, RootedFs},
    status_not_found,
    storage::{commit_staged_file, sync_file_to_storage},
};
use crate::{
    http_utils::{IncomingStream, body_full},
    utils::get_file_name,
};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use futures_util::{Stream, TryStreamExt, pin_mut};
use headers::{ContentLength, HeaderMap, HeaderMapExt};
use hyper::{StatusCode, header::HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fmt,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::{
    fs,
    io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    time::{Instant, timeout_at},
};
use uuid::Uuid;

const RESUMABLE_UPLOAD_MIN_SIZE: u64 = 20 * 1024 * 1024;
const UPLOAD_SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const TRASH_ENTRY_TTL: Duration = Duration::from_secs(60 * 60);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_ID_HEADER: &str = "x-dufs-upload-id";
const UPLOAD_LENGTH_HEADER: &str = "x-dufs-upload-length";
const UPLOAD_OFFSET_HEADER: &str = "x-dufs-upload-offset";
const UPLOAD_TEMP_PREFIX: &str = ".dufs-upload-";
const UPLOAD_TEMP_SUFFIX: &str = ".part";
const UPLOAD_STATE_SUFFIX: &str = ".state";
const UPLOAD_STATE_TEMP_SUFFIX: &str = ".tmp";
const DELETE_TRASH_PREFIX: &str = ".dufs-upload-delete-";
const DELETE_TRASH_SUFFIX: &str = ".trash";
const UPLOAD_STATE_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InternalUploadName {
    Stage,
    State,
    StateTemp,
    DeleteTrash,
}

#[derive(Debug)]
enum UploadTransferError {
    Io(io::Error),
    IdleTimeout,
    TotalTimeout,
    ExcessBody,
    InsufficientStorage,
}

struct UploadTempCleanup {
    rooted_fs: RootedFs,
    path: PathBuf,
    enabled: bool,
}

pub(super) struct UploadOptions {
    pub(super) resume: bool,
    pub(super) upload_id: Uuid,
    pub(super) upload_length: u64,
    pub(super) upload_offset: Option<u64>,
    pub(super) path_lease: PathLease,
}

struct ActiveUploadFilesLease {
    active: Arc<Mutex<HashSet<RootedEntryKey>>>,
    keys: Vec<RootedEntryKey>,
}

#[derive(Debug, Deserialize, Serialize)]
struct UploadCheckpoint {
    version: u8,
    stage_name: String,
    upload_length: u64,
    durable_offset: u64,
}

impl fmt::Display for UploadTransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::IdleTimeout => formatter.write_str("upload body idle timeout"),
            Self::TotalTimeout => formatter.write_str("upload body total timeout"),
            Self::ExcessBody => {
                formatter.write_str("request body exceeds the declared remaining upload length")
            }
            Self::InsufficientStorage => {
                formatter.write_str("upload would consume the protected free disk space")
            }
        }
    }
}

impl std::error::Error for UploadTransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl Drop for UploadTempCleanup {
    fn drop(&mut self) {
        if self.enabled {
            let _ = self.rooted_fs.remove_file_if_exists_blocking(&self.path);
        }
    }
}

impl Drop for ActiveUploadFilesLease {
    fn drop(&mut self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in &self.keys {
            active.remove(key);
        }
    }
}

impl Server {
    async fn track_active_upload_files(
        &self,
        upload_path: &Path,
    ) -> std::io::Result<ActiveUploadFilesLease> {
        let paths = vec![
            upload_path.to_path_buf(),
            upload_state_path(upload_path).map_err(io::Error::other)?,
        ];
        let mut keys = Vec::with_capacity(paths.len());
        for path in paths {
            keys.push(self.rooted_fs.entry_key(&path).await?);
        }
        let mut active = self
            .active_upload_files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in &keys {
            active.insert(key.clone());
        }
        drop(active);
        Ok(ActiveUploadFilesLease {
            active: self.active_upload_files.clone(),
            keys,
        })
    }

    pub(super) async fn run_upload_maintenance(self: Arc<Self>) {
        if let Err(err) = self.cleanup_stale_internal_files(Duration::ZERO).await {
            warn!("Failed to clean internal files at startup error={err:#}");
        }
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                _ = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
            }
            if let Err(err) = self.cleanup_stale_internal_files(TRASH_ENTRY_TTL).await {
                warn!("Failed to clean stale internal files error={err:#}");
            }
        }
    }

    async fn cleanup_stale_internal_files(&self, trash_ttl: Duration) -> Result<()> {
        let root = self.args.serve_path.clone();
        let rooted_fs = self.rooted_fs.clone();
        let active = self.active_upload_files.clone();
        let shutdown = self.shutdown.clone();
        let removed = self
            .work_tasks
            .spawn_blocking(move || {
                collect_and_remove_stale_internal_files(
                    &rooted_fs,
                    &root,
                    &active,
                    SystemTime::now(),
                    UPLOAD_SESSION_TTL,
                    trash_ttl,
                    Some(&shutdown),
                )
            })
            .await??;

        for path in removed {
            self.rooted_fs.sync_parent(&path).await?;
            info!("Removed stale internal file path={}", path.display());
        }
        Ok(())
    }

    pub(super) async fn handle_upload_status(
        &self,
        path: &Path,
        upload_id: Uuid,
        res: &mut Response,
    ) -> Result<()> {
        let upload_path = upload_temp_path(path, upload_id)?;
        let Some(checkpoint) = load_upload_checkpoint(&self.rooted_fs, &upload_path).await? else {
            status_not_found(res);
            return Ok(());
        };
        res.headers_mut()
            .typed_insert(ContentLength(checkpoint.durable_offset));
        res.headers_mut().insert(
            "x-dufs-upload-offset",
            HeaderValue::from_str(&checkpoint.durable_offset.to_string())?,
        );
        res.headers_mut().insert(
            UPLOAD_LENGTH_HEADER,
            HeaderValue::from_str(&checkpoint.upload_length.to_string())?,
        );
        Ok(())
    }

    pub(super) async fn handle_upload(
        &self,
        path: &Path,
        options: UploadOptions,
        req: Request,
        res: &mut Response,
    ) -> Result<()> {
        let UploadOptions {
            resume,
            upload_id,
            upload_length,
            upload_offset,
            path_lease,
        } = options;
        if upload_length > self.args.max_upload_size {
            *res.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
            *res.body_mut() = body_full(format!(
                "Upload length {upload_length} exceeds the configured maximum of {} bytes",
                self.args.max_upload_size
            ));
            return Ok(());
        }
        let request_body_length = req
            .headers()
            .typed_get::<ContentLength>()
            .map(|value| value.0);
        let upload_path = upload_temp_path(path, upload_id)?;
        let active_upload_files = if resume {
            match self.track_active_upload_files(&upload_path).await {
                Ok(lease) => lease,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    status_not_found(res);
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            self.rooted_fs.ensure_parent(&upload_path).await?;
            self.track_active_upload_files(&upload_path).await?
        };
        let target_permissions = self
            .rooted_fs
            .metadata_nofollow(path)
            .await
            .ok()
            .filter(|metadata| metadata.file_type().is_file())
            .map(|metadata| metadata.permissions());
        let session_checkpoint = if resume {
            let Some(checkpoint) = load_upload_checkpoint(&self.rooted_fs, &upload_path).await?
            else {
                status_not_found(res);
                return Ok(());
            };
            Some(checkpoint)
        } else {
            None
        };
        let upload_offset = if resume {
            Some(upload_offset.expect("PATCH validated an upload offset"))
        } else {
            None
        };
        let initial_offset = upload_offset.unwrap_or_default();
        if let Some(checkpoint) = session_checkpoint.as_ref() {
            if checkpoint.upload_length != upload_length {
                *res.status_mut() = StatusCode::CONFLICT;
                *res.body_mut() = body_full(format!(
                    "Upload length changed: expected {}, received {upload_length}",
                    checkpoint.upload_length,
                ));
                return Ok(());
            }
            if checkpoint.durable_offset != initial_offset {
                *res.status_mut() = StatusCode::CONFLICT;
                *res.body_mut() = body_full("Upload offset changed; query it again");
                return Ok(());
            }
        }
        let remaining = upload_length.checked_sub(initial_offset).ok_or_else(|| {
            anyhow!("Upload offset {initial_offset} exceeds total length {upload_length}")
        })?;
        if request_body_length.is_some_and(|body_length| body_length > remaining) {
            *res.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
            *res.body_mut() = body_full("Request body exceeds declared remaining upload length");
            return Ok(());
        }

        let (mut file, status) = match upload_offset {
            None => {
                reset_upload_session(&self.rooted_fs, &upload_path).await?;
                let mut file = create_upload_temp(&self.rooted_fs, &upload_path).await?;
                persist_upload_checkpoint(
                    &self.rooted_fs,
                    &mut file,
                    &upload_path,
                    upload_length,
                    0,
                )
                .await?;
                (file, StatusCode::CREATED)
            }
            Some(offset) => {
                let checkpoint = session_checkpoint.expect("loaded upload checkpoint");
                debug_assert_eq!(checkpoint.upload_length, upload_length);
                debug_assert_eq!(checkpoint.durable_offset, offset);
                let metadata = self.rooted_fs.metadata_nofollow(&upload_path).await?;
                if !metadata.file_type().is_file() || metadata.len() < offset {
                    *res.status_mut() = StatusCode::CONFLICT;
                    *res.body_mut() =
                        body_full("Upload staging file is invalid; restart the upload");
                    return Ok(());
                }
                let mut file = self.rooted_fs.open_write(&upload_path).await?;
                if metadata.len() > offset {
                    file.set_len(offset).await?;
                }
                file.seek(SeekFrom::Start(offset)).await?;
                (file, StatusCode::NO_CONTENT)
            }
        };

        let Some(space_lease) =
            self.disk_space
                .reserve(&file, remaining, self.args.min_free_space)?
        else {
            drop(file);
            if !resume {
                reset_upload_session(&self.rooted_fs, &upload_path).await?;
            }
            *res.status_mut() = StatusCode::INSUFFICIENT_STORAGE;
            *res.body_mut() = body_full("Insufficient protected free disk space");
            return Ok(());
        };

        let transfer_result = receive_upload_body(
            IncomingStream::new(req.into_body()),
            &mut file,
            UploadTransferOptions {
                remaining,
                space_lease,
                minimum_free: self.args.min_free_space,
                idle_timeout: Duration::from_secs(self.args.upload_idle_timeout),
                total_timeout: Duration::from_secs(self.args.upload_total_timeout),
                force_shutdown: &self.force_shutdown,
            },
        )
        .await;
        if let Err(err) = transfer_result {
            let partial_size = file
                .metadata()
                .await
                .map(|metadata| metadata.len())
                .unwrap_or_default();

            match err {
                UploadTransferError::ExcessBody => {
                    drop(file);
                    reset_upload_session(&self.rooted_fs, &upload_path).await?;
                    *res.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                    *res.body_mut() =
                        body_full("Request body exceeds declared remaining upload length");
                    return Ok(());
                }
                UploadTransferError::InsufficientStorage => {
                    file.set_len(initial_offset).await?;
                    sync_file_to_storage(&file).await?;
                    drop(file);
                    *res.status_mut() = StatusCode::INSUFFICIENT_STORAGE;
                    *res.body_mut() = body_full("Insufficient protected free disk space");
                    return Ok(());
                }
                UploadTransferError::IdleTimeout | UploadTransferError::TotalTimeout => {
                    let keep_for_resume =
                        partial_size >= RESUMABLE_UPLOAD_MIN_SIZE && partial_size <= upload_length;
                    if keep_for_resume {
                        persist_upload_checkpoint(
                            &self.rooted_fs,
                            &mut file,
                            &upload_path,
                            upload_length,
                            partial_size,
                        )
                        .await?;
                        res.headers_mut().insert(
                            UPLOAD_OFFSET_HEADER,
                            HeaderValue::from_str(&partial_size.to_string())?,
                        );
                    } else {
                        drop(file);
                        reset_upload_session(&self.rooted_fs, &upload_path).await?;
                    }
                    *res.status_mut() = StatusCode::REQUEST_TIMEOUT;
                    *res.body_mut() = body_full(err.to_string());
                    return Ok(());
                }
                UploadTransferError::Io(err) => {
                    let keep_for_resume =
                        partial_size >= RESUMABLE_UPLOAD_MIN_SIZE && partial_size <= upload_length;
                    if keep_for_resume {
                        persist_upload_checkpoint(
                            &self.rooted_fs,
                            &mut file,
                            &upload_path,
                            upload_length,
                            partial_size,
                        )
                        .await?;
                    } else {
                        drop(file);
                        reset_upload_session(&self.rooted_fs, &upload_path).await?;
                    }
                    return Err(err.into());
                }
            }
        }

        // Tokio may still have a blocking write queued after `write_all`
        // resolves. Wait for that queue before using metadata as the upload
        // protocol's authoritative byte count.
        file.flush().await?;
        let actual_length = file.metadata().await?.len();
        if actual_length != upload_length {
            if actual_length < upload_length {
                persist_upload_checkpoint(
                    &self.rooted_fs,
                    &mut file,
                    &upload_path,
                    upload_length,
                    actual_length,
                )
                .await?;
                res.headers_mut().insert(
                    "x-dufs-upload-offset",
                    HeaderValue::from_str(&actual_length.to_string())?,
                );
            } else {
                drop(file);
                reset_upload_session(&self.rooted_fs, &upload_path).await?;
            }
            *res.status_mut() = StatusCode::CONFLICT;
            *res.body_mut() = body_full(format!(
                "Upload is incomplete: expected {upload_length} bytes, stored {actual_length}"
            ));
            return Ok(());
        }

        if let Some(permissions) = target_permissions {
            file.set_permissions(permissions).await?;
        }
        file.flush().await?;
        // The complete upload handler runs as a tracked mutation task, so these
        // guards remain alive even if Hyper drops the request future after a
        // browser or gateway disconnect. The final rename and directory sync
        // therefore cannot outlive their path and maintenance leases.
        let _path_lease = path_lease;
        let _active_upload_files = active_upload_files;
        commit_staged_file(&self.storage, file, &upload_path, path).await?;

        if let Err(err) = remove_upload_checkpoint(&self.rooted_fs, &upload_path).await {
            warn!(
                "Upload committed but checkpoint cleanup failed target={} upload_id={} error={err:#}",
                path.display(),
                upload_id,
            );
        }

        res.headers_mut().insert(
            UPLOAD_ID_HEADER,
            HeaderValue::from_str(&upload_id.to_string())?,
        );
        *res.status_mut() = status;
        Ok(())
    }
}

struct UploadTransferOptions<'a> {
    remaining: u64,
    space_lease: DiskSpaceReservation,
    minimum_free: u64,
    idle_timeout: Duration,
    total_timeout: Duration,
    force_shutdown: &'a tokio_util::sync::CancellationToken,
}

async fn receive_upload_body<S>(
    stream: S,
    file: &mut fs::File,
    options: UploadTransferOptions<'_>,
) -> std::result::Result<(), UploadTransferError>
where
    S: Stream<Item = Result<Bytes>>,
{
    let UploadTransferOptions {
        mut remaining,
        mut space_lease,
        minimum_free,
        idle_timeout,
        total_timeout,
        force_shutdown,
    } = options;
    let started = Instant::now();
    let total_deadline = started.checked_add(total_timeout).ok_or_else(|| {
        UploadTransferError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upload total timeout is too large",
        ))
    })?;
    let mut idle_deadline = started.checked_add(idle_timeout).ok_or_else(|| {
        UploadTransferError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upload idle timeout is too large",
        ))
    })?;
    pin_mut!(stream);

    loop {
        let deadline = total_deadline.min(idle_deadline);
        let mut stream_ref = stream.as_mut();
        let next_frame = stream_ref.try_next();
        let next = tokio::select! {
            biased;
            _ = force_shutdown.cancelled() => {
                return Err(UploadTransferError::Io(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "upload interrupted by forced shutdown",
                )));
            }
            result = timeout_at(deadline, next_frame) => {
                match result {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => {
                        return Err(UploadTransferError::Io(io::Error::other(error)));
                    }
                    Err(_) if Instant::now() >= total_deadline => {
                        return Err(UploadTransferError::TotalTimeout);
                    }
                    Err(_) => return Err(UploadTransferError::IdleTimeout),
                }
            }
        };

        let Some(chunk) = next else {
            return Ok(());
        };
        if chunk.is_empty() {
            continue;
        }

        let chunk_len = u64::try_from(chunk.len()).map_err(|_| {
            UploadTransferError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "upload chunk length does not fit in u64",
            ))
        })?;
        let accepted_bytes = remaining.min(chunk_len);
        if accepted_bytes > 0 {
            let accepted_len = usize::try_from(accepted_bytes).map_err(|_| {
                UploadTransferError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "accepted upload chunk length does not fit in usize",
                ))
            })?;
            write_upload_chunk(&mut space_lease, file, &chunk[..accepted_len], minimum_free)
                .await?;
            remaining -= accepted_bytes;
            idle_deadline = Instant::now().checked_add(idle_timeout).ok_or_else(|| {
                UploadTransferError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "upload idle timeout is too large",
                ))
            })?;
            if Instant::now() >= total_deadline {
                return Err(UploadTransferError::TotalTimeout);
            }
        }
        if chunk_len > accepted_bytes {
            return Err(UploadTransferError::ExcessBody);
        }
    }
}

async fn write_upload_chunk(
    reservation: &mut DiskSpaceReservation,
    file: &mut fs::File,
    data: &[u8],
    minimum_free: u64,
) -> std::result::Result<(), UploadTransferError> {
    if data.is_empty() {
        return Ok(());
    }
    let data_len = u64::try_from(data.len()).map_err(|_| {
        UploadTransferError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upload chunk length does not fit in u64",
        ))
    })?;
    if data_len > reservation.remaining() {
        return Err(UploadTransferError::ExcessBody);
    }
    if !reservation
        .reserved_space_is_available(file, minimum_free)
        .map_err(UploadTransferError::Io)?
    {
        return Err(UploadTransferError::InsufficientStorage);
    }
    file.write_all(data)
        .await
        .map_err(UploadTransferError::Io)?;
    reservation.consume(data_len);
    Ok(())
}

fn collect_and_remove_stale_internal_files(
    rooted_fs: &RootedFs,
    root: &Path,
    active: &Mutex<HashSet<RootedEntryKey>>,
    now: SystemTime,
    upload_ttl: Duration,
    trash_ttl: Duration,
    shutdown: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        if shutdown.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            break;
        }
        rooted_fs.visit_dir_blocking(&directory, |entry| {
            if shutdown.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                return Ok(false);
            }
            // Internal files are always regular files or real directories.
            // Never follow a user-created symlink while doing maintenance.
            if entry.is_symlink {
                return Ok(true);
            }
            let is_dir = entry.metadata.is_dir();
            let Some(name) = entry.file_name.to_str() else {
                if is_dir {
                    directories.push(entry.path);
                }
                return Ok(true);
            };
            let Some(internal_name) = classify_internal_upload_name(name) else {
                if is_dir {
                    directories.push(entry.path);
                }
                return Ok(true);
            };
            let is_trash = internal_name == InternalUploadName::DeleteTrash;
            if is_dir && !is_trash {
                warn!(
                    "Refusing to recursively remove an invalid upload session directory path={}",
                    entry.path.display()
                );
                return Ok(true);
            }
            let age = entry
                .metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .unwrap_or_default();
            let ttl = if is_trash { trash_ttl } else { upload_ttl };
            if age < ttl {
                return Ok(true);
            }

            let result = if is_trash {
                rooted_fs.remove_entry_blocking(&entry.path, is_dir)
            } else {
                let active = active
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                if active.contains(&entry_key) {
                    return Ok(true);
                }
                rooted_fs.remove_entry_blocking(&entry.path, is_dir)
            };
            match result {
                Ok(true) => removed.push(entry.path),
                Ok(false) => {}
                Err(err) => {
                    warn!(
                        "Failed to remove stale internal file path={} error={err}",
                        entry.path.display()
                    );
                }
            }
            Ok(true)
        })?;
        if shutdown.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            break;
        }
    }
    Ok(removed)
}

fn upload_temp_path(path: &Path, upload_id: Uuid) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Upload target has no parent directory"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Upload target has no file name"))?;
    let digest = Sha256::digest(file_name.to_string_lossy().as_bytes());
    let target_tag = hex::encode(digest);
    Ok(parent.join(format!(
        "{UPLOAD_TEMP_PREFIX}{target_tag}-{upload_id}{UPLOAD_TEMP_SUFFIX}"
    )))
}

pub(super) fn is_upload_temp_name(file_name: &str) -> bool {
    classify_internal_upload_name(file_name).is_some()
}

fn classify_internal_upload_name(file_name: &str) -> Option<InternalUploadName> {
    if is_delete_trash_name(file_name) {
        return Some(InternalUploadName::DeleteTrash);
    }
    if is_upload_state_temp_name(file_name) {
        return Some(InternalUploadName::StateTemp);
    }
    if file_name
        .strip_suffix(UPLOAD_STATE_SUFFIX)
        .is_some_and(is_upload_stage_name)
    {
        return Some(InternalUploadName::State);
    }
    is_upload_stage_name(file_name).then_some(InternalUploadName::Stage)
}

fn is_upload_stage_name(file_name: &str) -> bool {
    let Some(value) = file_name
        .strip_prefix(UPLOAD_TEMP_PREFIX)
        .and_then(|value| value.strip_suffix(UPLOAD_TEMP_SUFFIX))
    else {
        return false;
    };
    let Some(target_tag) = value.get(..64) else {
        return false;
    };
    if !target_tag
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    value.get(64..65) == Some("-") && value.get(65..).is_some_and(is_canonical_uuid)
}

fn is_upload_state_temp_name(file_name: &str) -> bool {
    let Some(value) = file_name.strip_suffix(UPLOAD_STATE_TEMP_SUFFIX) else {
        return false;
    };
    let Some(separator_index) = value.len().checked_sub(37) else {
        return false;
    };
    value.get(separator_index..separator_index + 1) == Some("-")
        && value
            .get(..separator_index)
            .and_then(|state_name| state_name.strip_suffix(UPLOAD_STATE_SUFFIX))
            .is_some_and(is_upload_stage_name)
        && value
            .get(separator_index + 1..)
            .is_some_and(is_canonical_uuid)
}

fn is_delete_trash_name(file_name: &str) -> bool {
    file_name
        .strip_prefix(DELETE_TRASH_PREFIX)
        .and_then(|value| value.strip_suffix(DELETE_TRASH_SUFFIX))
        .is_some_and(is_canonical_uuid)
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|uuid| uuid.hyphenated().to_string() == value)
}

fn upload_state_path(upload_path: &Path) -> Result<PathBuf> {
    let file_name = upload_path
        .file_name()
        .ok_or_else(|| anyhow!("Upload staging path has no file name"))?;
    Ok(upload_path.with_file_name(format!(
        "{}{UPLOAD_STATE_SUFFIX}",
        file_name.to_string_lossy()
    )))
}

async fn create_upload_temp(rooted_fs: &RootedFs, path: &Path) -> Result<fs::File> {
    let (file, _) = rooted_fs.create_new(path).await?;
    Ok(file)
}

async fn load_upload_checkpoint(
    rooted_fs: &RootedFs,
    upload_path: &Path,
) -> Result<Option<UploadCheckpoint>> {
    let state_path = upload_state_path(upload_path)?;
    let state_metadata = match rooted_fs.metadata_nofollow(&state_path).await {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if state_metadata.len() > 4096 {
        return Ok(None);
    }

    let data = match rooted_fs.open_read_nofollow(&state_path).await {
        Ok(mut file) => {
            let mut data = Vec::with_capacity(state_metadata.len() as usize);
            file.read_to_end(&mut data).await?;
            data
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let checkpoint: UploadCheckpoint = match serde_json::from_slice(&data) {
        Ok(checkpoint) => checkpoint,
        Err(_) => return Ok(None),
    };
    if checkpoint.version != UPLOAD_STATE_VERSION
        || checkpoint.stage_name != get_file_name(upload_path)
        || checkpoint.durable_offset > checkpoint.upload_length
    {
        return Ok(None);
    }

    let stage_metadata = match rooted_fs.metadata_nofollow(upload_path).await {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if stage_metadata.len() < checkpoint.durable_offset {
        return Ok(None);
    }
    Ok(Some(checkpoint))
}

async fn persist_upload_checkpoint(
    rooted_fs: &RootedFs,
    file: &mut fs::File,
    upload_path: &Path,
    upload_length: u64,
    durable_offset: u64,
) -> Result<()> {
    if durable_offset > upload_length {
        return Err(anyhow!("Upload checkpoint exceeds declared length"));
    }
    file.flush().await?;
    sync_file_to_storage(file).await?;

    let state_path = upload_state_path(upload_path)?;
    let parent = state_path
        .parent()
        .ok_or_else(|| anyhow!("Upload checkpoint has no parent directory"))?;
    let state_name = state_path
        .file_name()
        .ok_or_else(|| anyhow!("Upload checkpoint has no file name"))?
        .to_string_lossy();
    let state_temp_path = parent.join(format!(
        "{state_name}-{}{UPLOAD_STATE_TEMP_SUFFIX}",
        Uuid::new_v4()
    ));
    let checkpoint = UploadCheckpoint {
        version: UPLOAD_STATE_VERSION,
        stage_name: get_file_name(upload_path).to_string(),
        upload_length,
        durable_offset,
    };
    let data = serde_json::to_vec(&checkpoint)?;

    let mut state_file = create_upload_temp(rooted_fs, &state_temp_path).await?;
    let mut state_temp_cleanup = UploadTempCleanup {
        rooted_fs: rooted_fs.clone(),
        path: state_temp_path.clone(),
        enabled: true,
    };
    state_file.write_all(&data).await?;
    state_file.flush().await?;
    sync_file_to_storage(&state_file).await?;
    drop(state_file);
    rooted_fs
        .rename_replace(&state_temp_path, &state_path)
        .await?;
    state_temp_cleanup.enabled = false;
    Ok(())
}

async fn remove_upload_checkpoint(rooted_fs: &RootedFs, upload_path: &Path) -> Result<()> {
    let state_path = upload_state_path(upload_path)?;
    rooted_fs.remove_file_if_exists_durable(&state_path).await?;
    Ok(())
}

async fn reset_upload_session(rooted_fs: &RootedFs, upload_path: &Path) -> Result<()> {
    let state_path = upload_state_path(upload_path)?;
    rooted_fs.remove_file_if_exists_durable(&state_path).await?;
    rooted_fs.remove_file_if_exists_durable(upload_path).await?;
    Ok(())
}

pub(super) fn parse_upload_id(headers: &HeaderMap<HeaderValue>) -> Result<Option<Uuid>> {
    let Some(value) = headers.get(UPLOAD_ID_HEADER) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| anyhow!("Invalid {UPLOAD_ID_HEADER} header"))?;
    Uuid::parse_str(value)
        .map(Some)
        .map_err(|_| anyhow!("Invalid {UPLOAD_ID_HEADER} header"))
}

pub(super) fn parse_upload_length(headers: &HeaderMap<HeaderValue>) -> Result<Option<u64>> {
    let Some(value) = headers.get(UPLOAD_LENGTH_HEADER) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| anyhow!("Invalid {UPLOAD_LENGTH_HEADER} header"))?;
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| anyhow!("Invalid {UPLOAD_LENGTH_HEADER} header"))
}

pub(super) fn parse_upload_offset(headers: &HeaderMap<HeaderValue>) -> Result<Option<u64>> {
    let Some(value) = headers.get(UPLOAD_OFFSET_HEADER) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| anyhow!("Invalid {UPLOAD_OFFSET_HEADER} header"))?;
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| anyhow!("Invalid {UPLOAD_OFFSET_HEADER} header"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::disk_space::DiskSpaceTracker;
    use futures_util::stream;

    fn stage_name(target: &str, upload_id: Uuid) -> String {
        get_file_name(&upload_temp_path(Path::new(target), upload_id).unwrap()).to_string()
    }

    #[tokio::test]
    async fn upload_body_never_writes_beyond_the_declared_remaining_length() {
        let mut file = fs::File::from_std(tempfile::tempfile().unwrap());
        let tracker = DiskSpaceTracker::new();
        let lease = tracker.reserve(&file, 3, 0).unwrap().unwrap();
        let chunks = stream::iter([Ok::<_, anyhow::Error>(Bytes::from_static(b"abcdef"))]);
        let cancellation = tokio_util::sync::CancellationToken::new();

        let error = receive_upload_body(
            chunks,
            &mut file,
            UploadTransferOptions {
                remaining: 3,
                space_lease: lease,
                minimum_free: 0,
                idle_timeout: Duration::from_secs(30),
                total_timeout: Duration::from_secs(30),
                force_shutdown: &cancellation,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(error, UploadTransferError::ExcessBody));
        file.flush().await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 3);
        file.seek(SeekFrom::Start(0)).await.unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).await.unwrap();
        assert_eq!(contents, b"abc");
    }

    #[tokio::test]
    async fn upload_body_enforces_idle_and_total_deadlines() {
        let cancellation = tokio_util::sync::CancellationToken::new();

        let mut idle_file = fs::File::from_std(tempfile::tempfile().unwrap());
        let idle_tracker = DiskSpaceTracker::new();
        let idle_lease = idle_tracker.reserve(&idle_file, 1, 0).unwrap().unwrap();
        let idle_error = receive_upload_body(
            stream::pending::<Result<Bytes>>(),
            &mut idle_file,
            UploadTransferOptions {
                remaining: 1,
                space_lease: idle_lease,
                minimum_free: 0,
                idle_timeout: Duration::from_millis(20),
                total_timeout: Duration::from_secs(1),
                force_shutdown: &cancellation,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(idle_error, UploadTransferError::IdleTimeout));

        let mut total_file = fs::File::from_std(tempfile::tempfile().unwrap());
        let total_tracker = DiskSpaceTracker::new();
        let total_lease = total_tracker
            .reserve(&total_file, 1024, 0)
            .unwrap()
            .unwrap();
        let slow_chunks = stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some((Ok::<_, anyhow::Error>(Bytes::from_static(b"x")), ()))
        });
        let total_error = receive_upload_body(
            slow_chunks,
            &mut total_file,
            UploadTransferOptions {
                remaining: 1024,
                space_lease: total_lease,
                minimum_free: 0,
                idle_timeout: Duration::from_millis(500),
                total_timeout: Duration::from_millis(30),
                force_shutdown: &cancellation,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(total_error, UploadTransferError::TotalTimeout));
        assert!(total_file.metadata().await.unwrap().len() < 1024);
    }

    #[test]
    fn maintenance_removes_expired_sessions_and_trash_but_skips_active_files() {
        let temp = assert_fs::TempDir::new().unwrap();
        let stale_stage_name = stage_name("stale.txt", Uuid::new_v4());
        let stale = temp.path().join(&stale_stage_name);
        let stale_state = temp
            .path()
            .join(format!("{stale_stage_name}{UPLOAD_STATE_SUFFIX}"));
        let stale_state_temp = temp.path().join(format!(
            "{stale_stage_name}{UPLOAD_STATE_SUFFIX}-{}{UPLOAD_STATE_TEMP_SUFFIX}",
            Uuid::new_v4()
        ));
        let active_stage_name = stage_name("active.txt", Uuid::new_v4());
        let active_stage = temp.path().join(&active_stage_name);
        let active_state = temp
            .path()
            .join(format!("{active_stage_name}{UPLOAD_STATE_SUFFIX}"));
        let trash = temp.path().join(format!(
            "{DELETE_TRASH_PREFIX}{}{DELETE_TRASH_SUFFIX}",
            Uuid::new_v4()
        ));
        let invalid_session_directory = temp
            .path()
            .join(stage_name("invalid-directory.txt", Uuid::new_v4()));
        let ordinary = temp.path().join("ordinary.txt");
        std::fs::write(&stale, "stale").unwrap();
        std::fs::write(&stale_state, "stale-state").unwrap();
        std::fs::write(&stale_state_temp, "stale-state-temp").unwrap();
        std::fs::write(&active_stage, "active").unwrap();
        std::fs::write(&active_state, "active-state").unwrap();
        std::fs::create_dir(&trash).unwrap();
        std::fs::write(trash.join("file.txt"), "trash").unwrap();
        std::fs::create_dir(&invalid_session_directory).unwrap();
        std::fs::write(invalid_session_directory.join("keep.txt"), "invalid").unwrap();
        std::fs::write(&ordinary, "ordinary").unwrap();

        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let active = Mutex::new(HashSet::from([
            rooted_fs.entry_key_blocking(&active_stage).unwrap(),
            rooted_fs.entry_key_blocking(&active_state).unwrap(),
        ]));
        let removed = collect_and_remove_stale_internal_files(
            &rooted_fs,
            temp.path(),
            &active,
            SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
            UPLOAD_SESSION_TTL,
            Duration::ZERO,
            None,
        )
        .unwrap();

        assert!(removed.contains(&stale));
        assert!(removed.contains(&stale_state));
        assert!(removed.contains(&stale_state_temp));
        assert!(removed.contains(&trash));
        assert!(!stale.exists());
        assert!(!stale_state.exists());
        assert!(!stale_state_temp.exists());
        assert!(!trash.exists());
        assert!(active_stage.exists());
        assert!(active_state.exists());
        assert!(invalid_session_directory.exists());
        assert!(ordinary.exists());
    }

    #[test]
    fn maintenance_rechecks_the_live_lease_set_before_deleting() {
        let temp = assert_fs::TempDir::new().unwrap();
        let stage = temp.path().join(stage_name("race.txt", Uuid::new_v4()));
        std::fs::write(&stage, "stale").unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let stage_key = rooted_fs.entry_key_blocking(&stage).unwrap();
        let active = Arc::new(Mutex::new(HashSet::new()));
        let mut registration = active.lock().unwrap();

        let cleaner = {
            let root = temp.path().to_path_buf();
            let active = active.clone();
            std::thread::spawn(move || {
                collect_and_remove_stale_internal_files(
                    &rooted_fs,
                    &root,
                    &active,
                    SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
                    UPLOAD_SESSION_TTL,
                    Duration::ZERO,
                    None,
                )
                .unwrap()
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        registration.insert(stage_key);
        drop(registration);

        let removed = cleaner.join().unwrap();
        assert!(!removed.contains(&stage));
        assert!(stage.exists());
    }

    #[test]
    fn maintenance_recognizes_active_uploads_through_root_internal_symlink_aliases() {
        use std::os::unix::fs::symlink;

        let temp = assert_fs::TempDir::new().unwrap();
        let target = temp.path().join("target");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&target).unwrap();
        symlink("target", &alias).unwrap();

        let stage_name = stage_name("aliased.txt", Uuid::new_v4());
        let aliased_stage = alias.join(&stage_name);
        let aliased_state = alias.join(format!("{stage_name}{UPLOAD_STATE_SUFFIX}"));
        std::fs::write(&aliased_stage, "active-stage").unwrap();
        std::fs::write(&aliased_state, "active-state").unwrap();

        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let active = Mutex::new(HashSet::from([
            rooted_fs.entry_key_blocking(&aliased_stage).unwrap(),
            rooted_fs.entry_key_blocking(&aliased_state).unwrap(),
        ]));
        let removed = collect_and_remove_stale_internal_files(
            &rooted_fs,
            temp.path(),
            &active,
            SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
            UPLOAD_SESSION_TTL,
            Duration::ZERO,
            None,
        )
        .unwrap();

        assert!(removed.is_empty());
        assert_eq!(
            std::fs::read_to_string(target.join(&stage_name)).unwrap(),
            "active-stage"
        );
        assert_eq!(
            std::fs::read_to_string(target.join(format!("{stage_name}{UPLOAD_STATE_SUFFIX}")))
                .unwrap(),
            "active-state"
        );
    }

    #[test]
    fn invalid_prefixed_names_are_not_internal_or_removed() {
        let temp = assert_fs::TempDir::new().unwrap();
        let upload_id = Uuid::new_v4();
        let canonical_stage = stage_name("target.txt", upload_id);
        let uppercase_target_tag_stage = format!(
            "{UPLOAD_TEMP_PREFIX}{}-{upload_id}{UPLOAD_TEMP_SUFFIX}",
            "A".repeat(64)
        );
        let invalid_names = [
            ".dufs-upload-not-a-stage.part".to_string(),
            ".dufs-upload-delete-old.trash".to_string(),
            format!("{canonical_stage}.extra"),
            format!("{canonical_stage}{UPLOAD_STATE_SUFFIX}-invalid{UPLOAD_STATE_TEMP_SUFFIX}"),
            format!(
                "{DELETE_TRASH_PREFIX}{}{DELETE_TRASH_SUFFIX}.extra",
                Uuid::new_v4()
            ),
            uppercase_target_tag_stage,
        ];

        for name in &invalid_names {
            assert!(!is_upload_temp_name(name), "{name}");
            std::fs::write(temp.path().join(name), "ordinary").unwrap();
        }

        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let removed = collect_and_remove_stale_internal_files(
            &rooted_fs,
            temp.path(),
            &Mutex::new(HashSet::new()),
            SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
            UPLOAD_SESSION_TTL,
            Duration::ZERO,
            None,
        )
        .unwrap();

        assert!(removed.is_empty());
        for name in &invalid_names {
            assert_eq!(
                std::fs::read_to_string(temp.path().join(name)).unwrap(),
                "ordinary"
            );
        }
    }

    #[test]
    fn maintenance_stays_on_the_opened_root_after_path_replacement() {
        let temp = assert_fs::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let moved_root = root.with_extension("opened-root");
        let stage = stage_name("stale.txt", Uuid::new_v4());
        std::fs::write(root.join(&stage), "original").unwrap();
        let rooted_fs = RootedFs::new(&root).unwrap();

        std::fs::rename(&root, &moved_root).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(&stage), "replacement").unwrap();

        let result = collect_and_remove_stale_internal_files(
            &rooted_fs,
            &root,
            &Mutex::new(HashSet::new()),
            SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60),
            UPLOAD_SESSION_TTL,
            Duration::ZERO,
            None,
        );

        assert!(result.is_ok());
        assert!(!moved_root.join(&stage).exists());
        assert_eq!(
            std::fs::read_to_string(root.join(&stage)).unwrap(),
            "replacement"
        );
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::rename(&moved_root, &root).unwrap();
    }
}
