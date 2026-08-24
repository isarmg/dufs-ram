use super::{
    BUF_SIZE, Response, Server,
    blocking_io::{BlockingIoGate, blocking_io_gate},
    set_content_disposition, status_not_found,
};
use crate::utils::{ParsedRange, parse_range, try_get_file_name};

use crate::utils::encode_hex;
use anyhow::Result;
use bytes::Bytes;
use futures_util::{TryStreamExt, stream};
use headers::{
    AcceptRanges, CacheControl, ETag, HeaderMap, HeaderMapExt, IfMatch, IfModifiedSince,
    IfNoneMatch, IfUnmodifiedSince, LastModified,
};
use http_body_util::{BodyExt, StreamBody, combinators::BoxBody};
use hyper::{
    StatusCode,
    body::Frame,
    header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, IF_RANGE, RANGE},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, Metadata},
    io,
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    time::Duration,
};
use tokio::fs;

/// Maximum time to obtain the next file-backed response chunk, including both
/// blocking-I/O admission and the underlying filesystem read. Socket write
/// progress has a separate idle deadline at the connection boundary.
const DOWNLOAD_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

struct DownloadReadState {
    file: File,
    gate: BlockingIoGate,
    offset: u64,
    remaining: u64,
    idle_timeout: Duration,
}

impl Server {
    pub(super) async fn handle_send_file(
        &self,
        path: &Path,
        headers: &HeaderMap<HeaderValue>,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        let file = match self.content.rooted_fs.open_read(path).await {
            Ok(file) => file,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                status_not_found(res);
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        send_open_file(path, file, headers, head_only, res).await
    }
}

async fn send_open_file(
    path: &Path,
    file: fs::File,
    headers: &HeaderMap<HeaderValue>,
    head_only: bool,
    res: &mut Response,
) -> Result<()> {
    let file = file.into_std().await;
    send_open_file_with_gate(
        path,
        file,
        headers,
        head_only,
        res,
        blocking_io_gate().clone(),
        DOWNLOAD_READ_IDLE_TIMEOUT,
    )
    .await
}

async fn send_open_file_with_gate(
    path: &Path,
    file: File,
    headers: &HeaderMap<HeaderValue>,
    head_only: bool,
    res: &mut Response,
    gate: BlockingIoGate,
    read_idle_timeout: Duration,
) -> Result<()> {
    let (file, meta) = gate
        .run_io(move || {
            let meta = file.metadata()?;
            Ok((file, meta))
        })
        .await?;
    if !meta.is_file() {
        status_not_found(res);
        return Ok(());
    }
    let size = meta.len();
    res.headers_mut()
        .typed_insert(CacheControl::new().with_private().with_no_store());
    let mut use_range = !head_only;
    if let Some((etag, last_modified)) = extract_cache_headers(&meta) {
        res.headers_mut().typed_insert(last_modified);
        res.headers_mut().typed_insert(etag.clone());

        if let Some(if_match) = headers.typed_get::<IfMatch>() {
            if !if_match.precondition_passes(&etag) {
                *res.status_mut() = StatusCode::PRECONDITION_FAILED;
                return Ok(());
            }
        } else if let Some(if_unmodified_since) = headers.typed_get::<IfUnmodifiedSince>()
            && !if_unmodified_since.precondition_passes(last_modified.into())
        {
            *res.status_mut() = StatusCode::PRECONDITION_FAILED;
            return Ok(());
        }

        if let Some(if_none_match) = headers.typed_get::<IfNoneMatch>() {
            if !if_none_match.precondition_passes(&etag) {
                *res.status_mut() = StatusCode::NOT_MODIFIED;
                return Ok(());
            }
        } else if let Some(if_modified_since) = headers.typed_get::<IfModifiedSince>()
            && !if_modified_since.is_modified(last_modified.into())
        {
            *res.status_mut() = StatusCode::NOT_MODIFIED;
            return Ok(());
        }

        // A weak ETag cannot satisfy If-Range's required strong comparison,
        // and second-granularity Last-Modified is not a safe strong validator
        // for rapid atomic replacements. Send the complete representation
        // whenever If-Range is present.
        use_range = !head_only && headers.contains_key(RANGE) && !headers.contains_key(IF_RANGE);
    }

    let range = if use_range {
        let mut ranges = headers.get_all(RANGE).iter();
        ranges.next().map(|range| {
            let parsed = range
                .to_str()
                .map_or(ParsedRange::Unsatisfiable, |range| parse_range(range, size));
            // RFC 9110 requires an origin server to ignore a Range field whose
            // unit it does not understand. Preserve that decision before
            // applying this server's stricter policy for repeated byte-range
            // fields; otherwise two unknown-unit fields are incorrectly
            // converted into a 416 response.
            if parsed == ParsedRange::Ignore {
                return ParsedRange::Ignore;
            }
            if ranges.next().is_some() {
                return ParsedRange::Unsatisfiable;
            }
            parsed
        })
    } else {
        None
    };

    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&get_content_type(path))?,
    );

    let filename = try_get_file_name(path)?;
    set_content_disposition(res, filename)?;

    res.headers_mut().typed_insert(AcceptRanges::bytes());

    match range {
        Some(ParsedRange::Satisfiable(start, end)) => {
            let range_size = end - start + 1;
            *res.status_mut() = StatusCode::PARTIAL_CONTENT;
            let content_range = format!("bytes {start}-{end}/{size}");
            res.headers_mut()
                .insert(CONTENT_RANGE, content_range.parse()?);
            res.headers_mut()
                .insert(CONTENT_LENGTH, format!("{range_size}").parse()?);
            if head_only {
                return Ok(());
            }

            *res.body_mut() = gated_file_body(file, gate, start, range_size, read_idle_timeout);
        }
        Some(ParsedRange::Unsatisfiable) => {
            *res.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            res.headers_mut()
                .insert(CONTENT_RANGE, format!("bytes */{size}").parse()?);
        }
        None | Some(ParsedRange::Ignore) => {
            res.headers_mut()
                .insert(CONTENT_LENGTH, format!("{size}").parse()?);
            if head_only {
                return Ok(());
            }

            // Keep the body framed to the representation whose metadata produced
            // Content-Length, even if another writer appends to the same inode.
            *res.body_mut() = gated_file_body(file, gate, 0, size, read_idle_timeout);
        }
    }
    Ok(())
}

fn gated_file_body(
    file: File,
    gate: BlockingIoGate,
    offset: u64,
    remaining: u64,
    idle_timeout: Duration,
) -> BoxBody<Bytes, anyhow::Error> {
    let stream = stream::try_unfold(
        DownloadReadState {
            file,
            gate,
            offset,
            remaining,
            idle_timeout,
        },
        |state| async move {
            if state.remaining == 0 {
                return Ok(None);
            }

            let chunk_size = state.remaining.min(BUF_SIZE as u64) as usize;
            let idle_timeout = state.idle_timeout;
            let gate = state.gate.clone();
            let read = gate.run_io(move || {
                let mut buffer = vec![0_u8; chunk_size];
                let read = state.file.read_at(&mut buffer, state.offset)?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "download file ended before its advertised length",
                    ));
                }
                buffer.truncate(read);
                let read = read as u64;
                let next = DownloadReadState {
                    file: state.file,
                    gate: state.gate,
                    offset: state.offset + read,
                    remaining: state.remaining - read,
                    idle_timeout: state.idle_timeout,
                };
                Ok(Some((Bytes::from(buffer), next)))
            });
            match tokio::time::timeout(idle_timeout, read).await {
                Ok(result) => result,
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "download response body produced no file data before the read idle deadline",
                )),
            }
        },
    );
    StreamBody::new(stream.map_ok(Frame::data).map_err(anyhow::Error::from)).boxed()
}

fn extract_cache_headers(meta: &Metadata) -> Option<(ETag, LastModified)> {
    let mtime = meta.modified().ok().or_else(|| meta.created().ok())?;
    // File identity and nanosecond timestamps distinguish normal in-place
    // changes and same-size atomic replacements without claiming that
    // metadata is a collision-proof content digest, so emit a weak validator.
    let mut validator = Sha256::new();
    validator.update(meta.dev().to_le_bytes());
    validator.update(meta.ino().to_le_bytes());
    validator.update(meta.len().to_le_bytes());
    validator.update(meta.mtime().to_le_bytes());
    validator.update(meta.mtime_nsec().to_le_bytes());
    validator.update(meta.ctime().to_le_bytes());
    validator.update(meta.ctime_nsec().to_le_bytes());
    let validator = encode_hex(validator.finalize());
    let etag = format!(r#"W/"{validator}""#).parse::<ETag>().ok()?;
    let last_modified = LastModified::from(mtime);
    Some((etag, last_modified))
}

fn get_content_type(path: &Path) -> String {
    // Every file response is an attachment, so sampling its contents only to
    // guess a charset adds a read/seek round trip and two dependencies without
    // affecting browser rendering. Extension MIME data remains useful to
    // download managers; unknown extensions fail closed as generic bytes.
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::rooted_fs::RootedFs;
    use std::{
        future::{Future, poll_fn},
        io::Write as _,
        pin::Pin,
        sync::mpsc,
        task::Poll,
        time::Duration,
    };

    async fn render_open_file(
        path: &Path,
        file: fs::File,
        headers: HeaderMap<HeaderValue>,
    ) -> (StatusCode, HeaderMap<HeaderValue>, Vec<u8>) {
        let mut response = Response::default();
        send_open_file(path, file, &headers, false, &mut response)
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, body)
    }

    fn assert_cache_headers(headers: &HeaderMap<HeaderValue>, expected: &(ETag, LastModified)) {
        assert_eq!(headers.typed_get::<ETag>().as_ref(), Some(&expected.0));
        assert_eq!(
            headers.typed_get::<LastModified>().as_ref(),
            Some(&expected.1)
        );
    }

    async fn assert_future_pending<F>(mut future: Pin<&mut F>, message: &str)
    where
        F: Future,
    {
        let first_poll = poll_fn(|context| Poll::Ready(future.as_mut().poll(context))).await;
        assert!(first_poll.is_pending(), "{message}");
    }

    async fn occupy_gate(
        gate: &BlockingIoGate,
    ) -> (
        mpsc::Sender<()>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocker_gate = gate.clone();
        let blocker = tokio::spawn(async move {
            blocker_gate
                .run_io(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocking gate holder did not start")
        })
        .await
        .unwrap();
        (release_tx, blocker)
    }

    #[tokio::test]
    async fn open_file_metadata_waits_for_blocking_io_admission() {
        let temp = assert_fs::TempDir::new().unwrap();
        let target = temp.path().join("download.txt");
        std::fs::write(&target, "gated metadata").unwrap();
        let file = std::fs::File::open(&target).unwrap();
        let gate = BlockingIoGate::with_capacity_for_test(1);
        let (release, blocker) = occupy_gate(&gate).await;
        let headers = HeaderMap::new();
        let mut response = Response::default();
        let mut send = Box::pin(send_open_file_with_gate(
            &target,
            file,
            &headers,
            false,
            &mut response,
            gate,
            DOWNLOAD_READ_IDLE_TIMEOUT,
        ));

        assert_future_pending(
            send.as_mut(),
            "file metadata bypassed blocking I/O admission",
        )
        .await;
        release.send(()).unwrap();
        blocker
            .await
            .expect("gate holder task failed")
            .expect("gate holder I/O failed");
        send.as_mut().await.unwrap();
        drop(send);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_LENGTH], "14");
    }

    #[tokio::test]
    async fn range_body_reads_wait_for_blocking_io_admission() {
        let temp = assert_fs::TempDir::new().unwrap();
        let target = temp.path().join("download.bin");
        std::fs::write(&target, b"0123456789").unwrap();
        let file = std::fs::File::open(&target).unwrap();
        let gate = BlockingIoGate::with_capacity_for_test(1);
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=2-7"));
        let mut response = Response::default();
        send_open_file_with_gate(
            &target,
            file,
            &headers,
            false,
            &mut response,
            gate.clone(),
            DOWNLOAD_READ_IDLE_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);

        let (release, blocker) = occupy_gate(&gate).await;
        let mut collect = Box::pin(response.into_body().collect());
        assert_future_pending(
            collect.as_mut(),
            "range body read bypassed blocking I/O admission",
        )
        .await;
        release.send(()).unwrap();
        blocker
            .await
            .expect("gate holder task failed")
            .expect("gate holder I/O failed");
        let body = collect.await.unwrap().to_bytes();

        assert_eq!(body.as_ref(), b"234567");
    }

    #[tokio::test]
    async fn queued_download_read_expires_with_a_diagnostic_idle_timeout() {
        let temp = assert_fs::TempDir::new().unwrap();
        let target = temp.path().join("download.bin");
        std::fs::write(&target, b"content").unwrap();
        let file = std::fs::File::open(&target).unwrap();
        let gate = BlockingIoGate::with_capacity_for_test(1);
        let mut response = Response::default();
        send_open_file_with_gate(
            &target,
            file,
            &HeaderMap::new(),
            false,
            &mut response,
            gate.clone(),
            Duration::ZERO,
        )
        .await
        .unwrap();

        let (release, blocker) = occupy_gate(&gate).await;
        let error = match response.into_body().collect().await {
            Err(error) => error,
            Ok(_) => panic!("a queued download read must exceed its idle deadline"),
        };
        let source = error
            .downcast_ref::<io::Error>()
            .expect("download idle timeout must retain its I/O error type");
        assert_eq!(source.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            source.to_string(),
            "download response body produced no file data before the read idle deadline"
        );

        release.send(()).unwrap();
        blocker
            .await
            .expect("gate holder task failed")
            .expect("gate holder I/O failed");
        assert_eq!(gate.run_io(|| Ok(7_u8)).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn open_file_response_stays_consistent_across_atomic_replacement() {
        let temp = assert_fs::TempDir::new().unwrap();
        let target = temp.path().join("download.txt");
        let replacement = temp.path().join("replacement.txt");
        let old_body = b"old text representation";
        let new_body = vec![0_u8; 4096];
        std::fs::write(&target, old_body).unwrap();
        std::fs::write(&replacement, &new_body).unwrap();

        let old_file = fs::File::open(&target).await.unwrap();
        std::fs::rename(&replacement, &target).unwrap();
        let old_cache_headers = extract_cache_headers(&old_file.metadata().await.unwrap()).unwrap();

        let (old_status, old_headers, rendered_old_body) =
            render_open_file(&target, old_file, HeaderMap::new()).await;
        let new_file = fs::File::open(&target).await.unwrap();
        let new_cache_headers = extract_cache_headers(&new_file.metadata().await.unwrap()).unwrap();
        let (new_status, new_headers, rendered_new_body) =
            render_open_file(&target, new_file, HeaderMap::new()).await;

        assert_eq!(old_status, StatusCode::OK);
        assert_eq!(old_headers[CONTENT_LENGTH], old_body.len().to_string());
        assert_eq!(old_headers[CONTENT_TYPE], "text/plain");
        assert_cache_headers(&old_headers, &old_cache_headers);
        assert_eq!(rendered_old_body, old_body);

        assert_eq!(new_status, StatusCode::OK);
        assert_eq!(new_headers[CONTENT_LENGTH], new_body.len().to_string());
        assert_eq!(new_headers[CONTENT_TYPE], "text/plain");
        assert_cache_headers(&new_headers, &new_cache_headers);
        assert_eq!(rendered_new_body, new_body);

        assert_ne!(old_cache_headers.0, new_cache_headers.0);
        assert_eq!(std::fs::read(&target).unwrap(), new_body);
    }

    #[tokio::test]
    async fn substituted_fifo_is_classified_from_the_open_fd_without_blocking() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let root_fd = std::fs::File::open(temp.path()).unwrap();
        rustix::fs::mkfifoat(
            &root_fd,
            "download.txt",
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let path = temp.path().join("download.txt");

        let file = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rooted_fs.open_read(&path),
        )
        .await
        .expect("opening a substituted FIFO must not wait for a writer")
        .expect("open substituted FIFO for descriptor classification");
        let mut response = Response::default();
        send_open_file(&path, file, &HeaderMap::new(), false, &mut response)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn full_response_stops_at_open_file_length_after_in_place_append() {
        let temp = assert_fs::TempDir::new().unwrap();
        let target = temp.path().join("download.bin");
        let original_body = b"original representation";
        let appended_body = b" appended after response creation";
        std::fs::write(&target, original_body).unwrap();
        let original_inode = std::fs::metadata(&target).unwrap().ino();

        let file = fs::File::open(&target).await.unwrap();
        let mut response = Response::default();
        send_open_file(&target, file, &HeaderMap::new(), false, &mut response)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[CONTENT_LENGTH],
            original_body.len().to_string()
        );

        let mut appender = std::fs::OpenOptions::new()
            .append(true)
            .open(&target)
            .unwrap();
        appender.write_all(appended_body).unwrap();
        appender.sync_all().unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().ino(), original_inode);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), original_body);

        let mut expected_file = original_body.to_vec();
        expected_file.extend_from_slice(appended_body);
        assert_eq!(std::fs::read(&target).unwrap(), expected_file);
    }

    #[tokio::test]
    async fn range_response_uses_open_file_length_after_atomic_replacement() {
        let temp = assert_fs::TempDir::new().unwrap();
        let target = temp.path().join("download.txt");
        let replacement = temp.path().join("replacement.txt");
        let old_body = b"old text representation";
        let new_body = vec![0_u8; 4096];
        std::fs::write(&target, old_body).unwrap();
        std::fs::write(&replacement, &new_body).unwrap();

        let old_file = fs::File::open(&target).await.unwrap();
        std::fs::rename(&replacement, &target).unwrap();
        let old_cache_headers = extract_cache_headers(&old_file.metadata().await.unwrap()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=4-7"));

        let (status, headers, body) = render_open_file(&target, old_file, headers).await;

        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            headers[CONTENT_RANGE],
            format!("bytes 4-7/{}", old_body.len())
        );
        assert_eq!(headers[CONTENT_LENGTH], "4");
        assert_eq!(body, old_body[4..=7]);
        assert_cache_headers(&headers, &old_cache_headers);
        assert_eq!(std::fs::read(&target).unwrap(), new_body);
    }

    #[test]
    fn content_type_uses_extensions_and_fails_closed_for_unknown_names() {
        assert_eq!(get_content_type(Path::new("photo.jpg")), "image/jpeg");
        assert_eq!(get_content_type(Path::new("notes.txt")), "text/plain");
        assert_eq!(
            get_content_type(Path::new("no-extension")),
            "application/octet-stream"
        );
    }
}
