use bytes::Bytes;
use futures_util::Stream;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{
    HeaderMap,
    body::{Body, Incoming},
    header::CONTENT_TYPE,
};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug)]
pub struct IncomingStream {
    inner: Incoming,
}

impl IncomingStream {
    pub fn new(inner: Incoming) -> Self {
        Self { inner }
    }
}

impl Stream for IncomingStream {
    type Item = Result<Bytes, anyhow::Error>;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match futures_util::ready!(Pin::new(&mut self.inner).poll_frame(cx)?) {
                Some(frame) => match frame.into_data() {
                    Ok(data) => return Poll::Ready(Some(Ok(data))),
                    Err(_frame) => {}
                },
                None => return Poll::Ready(None),
            }
        }
    }
}

pub fn body_full(content: impl Into<hyper::body::Bytes>) -> BoxBody<Bytes, anyhow::Error> {
    Full::new(content.into())
        .map_err(anyhow::Error::new)
        .boxed()
}

/// Match one syntactically valid Content-Type field by its media-type essence.
/// Parameters are allowed, but duplicate fields and malformed parameters fail
/// closed instead of being hidden by a `split(';').next()` prefix check.
pub(crate) fn request_content_type_is(headers: &HeaderMap, expected: &str) -> bool {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<mime_guess::mime::Mime>().ok())
        .is_some_and(|value| value.essence_str().eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    #[test]
    fn content_type_matching_validates_the_complete_single_field() {
        let mut headers = HeaderMap::new();
        for (value, expected) in [
            ("application/json", true),
            ("Application/JSON; charset=utf-8", true),
            ("application/json; charset=\"utf-8\"", true),
            ("text/plain", false),
            ("application/json;garbage", false),
            ("application/json; charset", false),
            ("application/json;=utf-8", false),
        ] {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
            assert_eq!(
                request_content_type_is(&headers, "application/json"),
                expected,
                "unexpected result for {value}"
            );
        }

        headers.append(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(!request_content_type_is(&headers, "application/json"));
    }
}
