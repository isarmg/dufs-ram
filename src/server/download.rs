use super::{
    BUF_SIZE, ContentDispositionFallback, Response, Server, set_content_disposition,
    status_not_found,
};
use crate::{
    http_utils::LengthLimitedStream,
    utils::{parse_range, try_get_file_name},
};

use anyhow::{Result, anyhow};
use futures_util::TryStreamExt;
use headers::{
    AcceptRanges, CacheControl, ETag, HeaderMap, HeaderMapExt, IfMatch, IfModifiedSince,
    IfNoneMatch, IfUnmodifiedSince, LastModified,
};
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    StatusCode,
    body::Frame,
    header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, IF_RANGE, RANGE},
};
use sha2::{Digest, Sha256};
use std::{fs::Metadata, io::SeekFrom, os::unix::fs::MetadataExt, path::Path};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
};
use tokio_util::io::ReaderStream;

impl Server {
    pub(super) async fn handle_send_file(
        &self,
        path: &Path,
        headers: &HeaderMap<HeaderValue>,
        head_only: bool,
        res: &mut Response,
    ) -> Result<()> {
        let file = match self.rooted_fs.open_read(path).await {
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
    mut file: fs::File,
    headers: &HeaderMap<HeaderValue>,
    head_only: bool,
    res: &mut Response,
) -> Result<()> {
    let meta = file.metadata().await?;
    if !meta.is_file() {
        status_not_found(res);
        return Ok(());
    }
    let size = meta.len();
    res.headers_mut()
        .typed_insert(CacheControl::new().with_private().with_no_store());
    let mut use_range = true;
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
        use_range = headers.contains_key(RANGE) && !headers.contains_key(IF_RANGE);
    }

    let range = if use_range {
        headers.get(RANGE).map(|range| {
            range
                .to_str()
                .ok()
                .and_then(|range| parse_range(range, size))
        })
    } else {
        None
    };

    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&get_content_type(path, &mut file).await?)?,
    );

    let filename = try_get_file_name(path)?;
    set_content_disposition(res, filename, ContentDispositionFallback::File)?;

    res.headers_mut().typed_insert(AcceptRanges::bytes());

    if let Some(range) = range {
        if let Some((start, end)) = range {
            file.seek(SeekFrom::Start(start)).await?;
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

            let stream_body = StreamBody::new(
                LengthLimitedStream::new(file, range_size as usize)
                    .map_ok(Frame::data)
                    .map_err(|err| anyhow!("{err}")),
            );
            *res.body_mut() = stream_body.boxed();
        } else {
            *res.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            res.headers_mut()
                .insert(CONTENT_RANGE, format!("bytes */{size}").parse()?);
        }
    } else {
        res.headers_mut()
            .insert(CONTENT_LENGTH, format!("{size}").parse()?);
        if head_only {
            return Ok(());
        }

        let reader_stream = ReaderStream::with_capacity(file, BUF_SIZE);
        let stream_body = StreamBody::new(
            reader_stream
                .map_ok(Frame::data)
                .map_err(|err| anyhow!("{err}")),
        );
        *res.body_mut() = stream_body.boxed();
    }
    Ok(())
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
    let validator = hex::encode(validator.finalize());
    let etag = format!(r#"W/"{validator}""#).parse::<ETag>().ok()?;
    let last_modified = LastModified::from(mtime);
    Some((etag, last_modified))
}

async fn get_content_type(path: &Path, file: &mut fs::File) -> Result<String> {
    let mime = mime_guess::from_path(path).first();
    if let Some(mime) = &mime
        && !mime_may_need_charset(mime)
    {
        return Ok(mime.to_string());
    }

    let mut buffer: Vec<u8> = vec![];
    let mut sample = (&mut *file).take(1024);
    sample.read_to_end(&mut buffer).await?;
    drop(sample);
    file.seek(SeekFrom::Start(0)).await?;
    let is_text = content_inspector::inspect(&buffer).is_text();
    let content_type = if is_text {
        let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
        detector.feed(&buffer, buffer.len() < 1024);
        let enc = detector.guess(None, chardetng::Utf8Detection::Allow);
        let charset = format!("; charset={}", enc.name());
        match mime {
            Some(m) => format!("{m}{charset}"),
            None => format!("text/plain{charset}"),
        }
    } else {
        match mime {
            Some(m) => m.to_string(),
            None => "application/octet-stream".into(),
        }
    };
    Ok(content_type)
}

fn mime_may_need_charset(mime: &mime_guess::mime::Mime) -> bool {
    use mime_guess::mime;

    mime.type_() == mime::TEXT
        || matches!(
            mime.subtype(),
            mime::JSON | mime::JAVASCRIPT | mime::XML | mime::SVG | mime::HTML
        )
        || matches!(mime.suffix(), Some(mime::JSON | mime::XML))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(old_headers[CONTENT_TYPE], "text/plain; charset=UTF-8");
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
    fn known_binary_mime_types_do_not_require_content_sampling() {
        let jpeg = mime_guess::from_path("photo.jpg").first().unwrap();
        let zip = mime_guess::from_path("archive.zip").first().unwrap();
        let text = mime_guess::from_path("notes.txt").first().unwrap();
        let json = mime_guess::from_path("data.json").first().unwrap();

        assert!(!mime_may_need_charset(&jpeg));
        assert!(!mime_may_need_charset(&zip));
        assert!(mime_may_need_charset(&text));
        assert!(mime_may_need_charset(&json));
    }
}
