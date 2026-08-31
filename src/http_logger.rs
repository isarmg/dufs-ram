use std::{
    collections::HashMap,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use chrono::{Local, SecondsFormat};
use http_body_util::combinators::BoxBody;
use hyper::{
    Method, Request, Version,
    body::{Body, Frame, SizeHint},
    header::HeaderName,
};

use crate::{logger::BoundedLogLine, utils::decode_uri};

pub const DEFAULT_LOG_FORMAT: &str = r#"$time_iso8601 $log_level - $remote_addr "$request" $status operation_id=$operation_id operation_state=$operation_state"#;
const MAX_LOG_FORMAT_BYTES: usize = 4096;
const MAX_LOG_FORMAT_ELEMENTS: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub struct HttpLogger {
    elements: Vec<LogElement>,
    needs: LogNeeds,
}

impl Default for HttpLogger {
    fn default() -> Self {
        DEFAULT_LOG_FORMAT.parse().unwrap()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum LogElement {
    Variable(String),
    Header { name: HeaderName, sensitive: bool },
    Literal(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct LogNeeds(u16);

impl LogNeeds {
    const REQUEST: u16 = 1 << 0;
    const REQUEST_METHOD: u16 = 1 << 1;
    const REQUEST_URI: u16 = 1 << 2;
    const REMOTE_USER: u16 = 1 << 3;
    const TIME_LOCAL: u16 = 1 << 4;
    const TIME_ISO8601: u16 = 1 << 5;
    const MSEC: u16 = 1 << 6;
    const REMOTE_ADDR: u16 = 1 << 7;
    const STATUS: u16 = 1 << 8;
    const OPERATION_ID: u16 = 1 << 9;
    const OPERATION_STATE: u16 = 1 << 10;

    fn record(&mut self, name: &str) {
        self.0 |= Self::variable_flag(name).unwrap_or(0);
    }

    fn supports(name: &str) -> bool {
        Self::variable_flag(name).is_some()
    }

    fn variable_flag(name: &str) -> Option<u16> {
        Some(match name {
            "request" => Self::REQUEST,
            "request_method" => Self::REQUEST_METHOD,
            "request_uri" => Self::REQUEST_URI,
            "remote_user" => Self::REMOTE_USER,
            "time_local" => Self::TIME_LOCAL,
            "time_iso8601" => Self::TIME_ISO8601,
            "msec" => Self::MSEC,
            "remote_addr" => Self::REMOTE_ADDR,
            "status" => Self::STATUS,
            "operation_id" => Self::OPERATION_ID,
            "operation_state" => Self::OPERATION_STATE,
            "log_level" => 0,
            _ => return None,
        })
    }

    fn contains(self, flag: u16) -> bool {
        self.0 & flag != 0
    }
}

impl HttpLogger {
    pub fn data<B>(&self, req: &Request<B>) -> HashMap<String, String> {
        let mut data = HashMap::default();
        if self.needs.contains(LogNeeds::REQUEST) || self.needs.contains(LogNeeds::REQUEST_URI) {
            let uri = req.uri().to_string();
            if self.needs.contains(LogNeeds::REQUEST) {
                data.insert(
                    "request".to_string(),
                    format_request_line(req.method(), &uri, req.version()),
                );
            }
            if self.needs.contains(LogNeeds::REQUEST_URI) {
                data.insert("request_uri".to_string(), sanitize_request_uri(&uri));
            }
        }
        if self.needs.contains(LogNeeds::REQUEST_METHOD) {
            data.insert("request_method".to_string(), req.method().to_string());
        }
        for element in self.elements.iter() {
            match element {
                LogElement::Variable(_) => {}
                LogElement::Header { name, sensitive } => {
                    if *sensitive {
                        if req.headers().contains_key(name) {
                            data.insert(name.as_str().to_string(), "[REDACTED]".to_string());
                        }
                    } else if let Some(value) =
                        req.headers().get(name).and_then(|v| v.to_str().ok())
                    {
                        data.insert(name.as_str().to_string(), sanitize_log_value(value));
                    }
                }
                LogElement::Literal(_) => {}
            }
        }
        data
    }

    pub fn set_authenticated_user(&self, data: &mut HashMap<String, String>, user: &str) {
        if self.needs.contains(LogNeeds::REMOTE_USER) {
            data.insert("remote_user".to_string(), sanitize_log_value(user));
        }
    }

    /// Insert a runtime field only when the parsed format references it.
    /// The closure keeps address/status/UUID formatting off custom-log hot
    /// paths that do not emit those values.
    pub fn set_runtime_value<F>(
        &self,
        data: &mut HashMap<String, String>,
        name: &'static str,
        value: F,
    ) where
        F: FnOnce() -> String,
    {
        let required = match name {
            "remote_addr" => LogNeeds::REMOTE_ADDR,
            "status" => LogNeeds::STATUS,
            "operation_id" => LogNeeds::OPERATION_ID,
            "operation_state" => LogNeeds::OPERATION_STATE,
            _ => 0,
        };
        debug_assert_ne!(required, 0, "unknown access-log runtime field");
        if self.needs.contains(required) {
            data.insert(name.to_string(), value());
        }
    }

    pub fn log(&self, data: &HashMap<String, String>, err: Option<String>) {
        if self.elements.is_empty() {
            return;
        }
        let is_error = err.is_some();
        let output = self.render(data, err.as_deref());
        emit_http_access(&output, is_error);
    }

    /// Delay an access-log record until the response body finishes producing
    /// frames. Streaming failures and bodies dropped before completion are
    /// therefore recorded as errors instead of an eager successful response.
    pub(crate) fn log_response_body(
        &self,
        data: HashMap<String, String>,
        body: BoxBody<Bytes, anyhow::Error>,
        expected_body_bytes: Option<u64>,
        handler_error: Option<String>,
        omit_success: bool,
    ) -> BoxBody<Bytes, anyhow::Error> {
        if self.elements.is_empty() {
            return body;
        }

        let logger = self.clone();
        let completion: ResponseLogCompletion = Box::new(move |body_error| {
            let error = combine_response_errors(handler_error, body_error);
            if !omit_success || error.is_some() {
                logger.log(&data, error);
            }
        });
        let expected_body_bytes = expected_body_bytes.or_else(|| body.size_hint().exact());
        if body.is_end_stream() {
            completion(body_length_error(expected_body_bytes, 0));
            body
        } else {
            BoxBody::new(AccessLogBody::new(body, expected_body_bytes, completion))
        }
    }

    fn render(&self, data: &HashMap<String, String>, err: Option<&str>) -> String {
        let wall_clock = (self.needs.contains(LogNeeds::TIME_LOCAL)
            || self.needs.contains(LogNeeds::TIME_ISO8601))
        .then(Local::now);
        let time_local = self.needs.contains(LogNeeds::TIME_LOCAL).then(|| {
            wall_clock
                .as_ref()
                .expect("wall clock is captured when a formatted time is required")
                .to_rfc3339_opts(SecondsFormat::Secs, false)
        });
        let time_iso8601 = self.needs.contains(LogNeeds::TIME_ISO8601).then(|| {
            wall_clock
                .as_ref()
                .expect("wall clock is captured when a formatted time is required")
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        });
        let msec = self.needs.contains(LogNeeds::MSEC).then(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| format!("{:.3}", duration.as_secs_f64()))
                .unwrap_or_default()
        });
        let log_level = if err.is_some() { "ERROR" } else { "INFO" };

        let mut output = BoundedLogLine::new();
        for element in self.elements.iter() {
            match element {
                LogElement::Literal(value) => output.push_str(value.as_str()),
                LogElement::Variable(name) => {
                    let resolved = match name.as_str() {
                        "time_local" => time_local.as_deref(),
                        "time_iso8601" => time_iso8601.as_deref(),
                        "msec" => msec.as_deref(),
                        "log_level" => Some(log_level),
                        _ => None,
                    };
                    let val = resolved
                        .or_else(|| data.get(name.as_str()).map(|v| v.as_str()))
                        .unwrap_or("-");
                    output.push_str(val);
                }
                LogElement::Header { name, .. } => output.push_str(
                    data.get(name.as_str())
                        .map(|value| value.as_str())
                        .unwrap_or("-"),
                ),
            }
        }
        if let Some(err) = err {
            output.push_str(" ");
            append_sanitized_log_value(&mut output, err);
        }
        output.finish()
    }
}

type ResponseLogCompletion = Box<dyn FnOnce(Option<String>) + Send + Sync + 'static>;

struct AccessLogBody {
    inner: BoxBody<Bytes, anyhow::Error>,
    expected_body_bytes: Option<u64>,
    produced_body_bytes: u64,
    completion: Option<ResponseLogCompletion>,
}

impl AccessLogBody {
    fn new(
        inner: BoxBody<Bytes, anyhow::Error>,
        expected_body_bytes: Option<u64>,
        completion: ResponseLogCompletion,
    ) -> Self {
        Self {
            inner,
            expected_body_bytes,
            produced_body_bytes: 0,
            completion: Some(completion),
        }
    }

    fn complete(&mut self, error: Option<String>) {
        if let Some(completion) = self.completion.take() {
            completion(error);
        }
    }
}

impl Body for AccessLogBody {
    type Data = Bytes;
    type Error = anyhow::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.produced_body_bytes =
                        this.produced_body_bytes.saturating_add(data.len() as u64);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.complete(Some(format!("response body stream failed: {error:#}")));
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.complete(body_length_error(
                    this.expected_body_bytes,
                    this.produced_body_bytes,
                ));
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for AccessLogBody {
    fn drop(&mut self) {
        let error = match body_length_error(self.expected_body_bytes, self.produced_body_bytes) {
            Some(error) => Some(error),
            None if self.expected_body_bytes.is_some() => None,
            None => Some("response body was dropped before stream completion".to_string()),
        };
        self.complete(error);
    }
}

fn body_length_error(expected: Option<u64>, produced: u64) -> Option<String> {
    expected
        .filter(|expected| *expected != produced)
        .map(|expected| {
            format!("response body produced {produced} bytes but declared {expected} bytes")
        })
}

fn combine_response_errors(
    handler_error: Option<String>,
    body_error: Option<String>,
) -> Option<String> {
    match (handler_error, body_error) {
        (Some(handler), Some(body)) => Some(format!("{handler}; {body}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

/// Emit via the `log` crate with target `http_access` so the system logger
/// prints the line verbatim (no extra timestamp/level prefix).
fn emit_http_access(msg: &str, is_error: bool) {
    let level = if is_error {
        log::Level::Error
    } else {
        log::Level::Info
    };
    log::logger().log(
        &log::Record::builder()
            .args(format_args!("{}", msg))
            .level(level)
            .target("http_access")
            .build(),
    );
}

impl FromStr for HttpLogger {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() > MAX_LOG_FORMAT_BYTES {
            return Err(anyhow::anyhow!(
                "HTTP log format exceeds the {MAX_LOG_FORMAT_BYTES}-byte limit"
            ));
        }
        let mut elements = vec![];
        let mut is_var = false;
        let mut cache = String::new();
        for c in format!("{s} ").chars() {
            if c == '$' {
                if !cache.is_empty() {
                    elements.push(LogElement::Literal(cache.to_string()));
                }
                cache.clear();
                is_var = true;
            } else if is_var && !(c.is_alphanumeric() || c == '_') {
                if let Some(value) = cache.strip_prefix("$http_") {
                    let normalized = value.to_ascii_lowercase().replace('_', "-");
                    let name = HeaderName::from_bytes(normalized.as_bytes()).map_err(|_| {
                        anyhow::anyhow!("Invalid HTTP request header log variable `{cache}`")
                    })?;
                    let sensitive = is_sensitive_header(&name);
                    elements.push(LogElement::Header { name, sensitive });
                } else if let Some(value) = cache.strip_prefix('$') {
                    if !LogNeeds::supports(value) {
                        return Err(anyhow::anyhow!("Unknown HTTP log variable `${value}`"));
                    }
                    elements.push(LogElement::Variable(value.to_string()));
                }
                cache.clear();
                is_var = false;
            }
            cache.push(c);
        }
        let cache = cache.trim();
        if !cache.is_empty() {
            elements.push(LogElement::Literal(cache.to_string()));
        }
        if elements.len() > MAX_LOG_FORMAT_ELEMENTS {
            return Err(anyhow::anyhow!(
                "HTTP log format exceeds the {MAX_LOG_FORMAT_ELEMENTS}-element limit"
            ));
        }
        let mut needs = LogNeeds::default();
        for element in &elements {
            if let LogElement::Variable(name) = element {
                needs.record(name);
            }
        }
        Ok(Self { elements, needs })
    }
}

fn is_sensitive_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "x-csrf-token"
    )
}

fn sanitize_log_value(s: &str) -> String {
    let mut output = String::with_capacity(s.len());
    for character in s.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{{{:x}}}", character as u32);
            }
            character => output.push(character),
        }
    }
    output
}

fn format_request_line(method: &Method, uri: &str, version: Version) -> String {
    format!("{method} {} {version:?}", sanitize_log_value(uri))
}

fn sanitize_request_uri(uri: &str) -> String {
    decode_uri(uri).map_or_else(
        || sanitize_log_value(uri),
        |decoded| sanitize_log_value(&decoded),
    )
}

fn append_sanitized_log_value(output: &mut BoundedLogLine, value: &str) {
    for character in value.chars() {
        if output.is_truncated() {
            break;
        }
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                let escaped = format!("\\u{{{:x}}}", character as u32);
                output.push_str(&escaped);
            }
            character => {
                let mut encoded = [0_u8; 4];
                output.push_str(character.encode_utf8(&mut encoded));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use http_body_util::{BodyExt, StreamBody};
    use hyper::Uri;
    use std::sync::mpsc;

    fn observed_body(
        body: BoxBody<Bytes, anyhow::Error>,
        expected_body_bytes: Option<u64>,
    ) -> (
        BoxBody<Bytes, anyhow::Error>,
        mpsc::Receiver<Option<String>>,
    ) {
        let (sender, receiver) = mpsc::channel();
        let completion: ResponseLogCompletion = Box::new(move |error| {
            sender.send(error).unwrap();
        });
        (
            BoxBody::new(AccessLogBody::new(body, expected_body_bytes, completion)),
            receiver,
        )
    }

    #[tokio::test]
    async fn response_body_completion_distinguishes_success_failure_and_drop() {
        let successful = crate::http_utils::body_full("complete");
        let (successful, success) = observed_body(successful, Some(8));
        assert_eq!(
            successful.collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"complete")
        );
        assert_eq!(success.recv().unwrap(), None);

        let failing = StreamBody::new(stream::iter([Err::<Frame<Bytes>, anyhow::Error>(
            anyhow::anyhow!("synthetic read failure"),
        )]))
        .boxed();
        let (failing, failure) = observed_body(failing, None);
        assert!(failing.collect().await.is_err());
        assert_eq!(
            failure.recv().unwrap().as_deref(),
            Some("response body stream failed: synthetic read failure")
        );

        let pending =
            StreamBody::new(stream::pending::<Result<Frame<Bytes>, anyhow::Error>>()).boxed();
        let (pending, dropped) = observed_body(pending, None);
        drop(pending);
        assert_eq!(
            dropped.recv().unwrap().as_deref(),
            Some("response body was dropped before stream completion")
        );

        // Hyper may stop polling after the declared Content-Length instead of
        // asking a body for one final EOF frame. Dropping at that exact byte
        // boundary is a completed response, not an aborted stream.
        let exact = crate::http_utils::body_full("complete");
        let (mut exact, exact_drop) = observed_body(exact, Some(8));
        let frame = std::future::poll_fn(|context| Pin::new(&mut exact).poll_frame(context))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.into_data().unwrap(), Bytes::from_static(b"complete"));
        drop(exact);
        assert_eq!(exact_drop.recv().unwrap(), None);
    }

    #[test]
    fn request_header_variables_have_one_canonical_identity() {
        for variants in [
            ["$http_cookie", "$http_COOKIE", "$http_CoOkIe"],
            [
                "$http_authorization",
                "$http_AUTHORIZATION",
                "$http_Authorization",
            ],
            [
                "$http_proxy_authorization",
                "$http_PROXY_AUTHORIZATION",
                "$http_Proxy_Authorization",
            ],
            [
                "$http_x_csrf_token",
                "$http_X_CSRF_TOKEN",
                "$http_X_CsRf_ToKeN",
            ],
            [
                "$http_x_request_id",
                "$http_X_REQUEST_ID",
                "$http_X_Request_Id",
            ],
        ] {
            let canonical = variants[0].parse::<HttpLogger>().unwrap();
            for variant in &variants[1..] {
                assert_eq!(variant.parse::<HttpLogger>().unwrap(), canonical);
            }
        }
    }

    #[test]
    fn invalid_request_header_variables_are_rejected() {
        for format in ["$http_", "$http_Cookié"] {
            let error = format.parse::<HttpLogger>().unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Invalid HTTP request header log variable"),
                "unexpected error for {format:?}: {error:#}"
            );
        }
    }

    #[test]
    fn unknown_fixed_variables_are_rejected() {
        for format in ["$stauts", "$request_path", "prefix $"] {
            let error = format.parse::<HttpLogger>().unwrap_err();
            assert!(
                error.to_string().contains("Unknown HTTP log variable"),
                "unexpected error for {format:?}: {error:#}"
            );
        }
    }

    #[test]
    fn oversized_or_overly_complex_log_formats_are_rejected() {
        let oversized = "文".repeat(MAX_LOG_FORMAT_BYTES / "文".len() + 1);
        let error = oversized.parse::<HttpLogger>().unwrap_err();
        assert!(error.to_string().contains("4096-byte limit"));

        let too_many_elements = "$status ".repeat(MAX_LOG_FORMAT_ELEMENTS);
        let error = too_many_elements.parse::<HttpLogger>().unwrap_err();
        assert!(error.to_string().contains("128-element limit"));
    }

    #[test]
    fn repeated_request_variables_render_directly_into_one_bounded_entry() {
        let format = std::iter::repeat_n("$request", MAX_LOG_FORMAT_ELEMENTS / 2)
            .collect::<Vec<_>>()
            .join(" ");
        let logger = format.parse::<HttpLogger>().unwrap();
        let mut data = HashMap::new();
        data.insert(
            "request".to_string(),
            format!("GET /{}", "中文".repeat(crate::logger::MAX_LOG_ENTRY_BYTES)),
        );

        let rendered = logger.render(&data, None);
        assert_eq!(rendered.len(), crate::logger::MAX_LOG_ENTRY_BYTES);
        assert!(rendered.ends_with(crate::logger::LOG_TRUNCATION_SUFFIX));
        assert_eq!(
            rendered
                .matches(crate::logger::LOG_TRUNCATION_SUFFIX)
                .count(),
            1
        );
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
    }

    #[test]
    fn dynamic_access_log_values_are_single_line_and_quoted() {
        assert_eq!(
            sanitize_log_value("line 1\r\n\"line 2\"\\tail"),
            "line 1\\r\\n\\\"line 2\\\"\\\\tail"
        );
    }

    #[test]
    fn complete_request_line_preserves_encoded_target_and_version() {
        let method = Method::GET;
        let uri = "/a%2Fb?value=%2F".parse::<Uri>().unwrap();
        assert_eq!(
            format_request_line(&method, &uri.to_string(), Version::HTTP_10),
            "GET /a%2Fb?value=%2F HTTP/1.0"
        );
        assert_eq!(
            format_request_line(&method, &uri.to_string(), Version::HTTP_11),
            "GET /a%2Fb?value=%2F HTTP/1.1"
        );
    }

    #[test]
    fn unused_runtime_fields_are_not_formatted_or_allocated() {
        let logger = "$request".parse::<HttpLogger>().unwrap();
        let formatted = std::cell::Cell::new(false);
        let mut data = HashMap::new();
        logger.set_runtime_value(&mut data, "status", || {
            formatted.set(true);
            "200".to_string()
        });
        assert!(!formatted.get());
        assert!(!data.contains_key("status"));

        let logger = "$status".parse::<HttpLogger>().unwrap();
        logger.set_runtime_value(&mut data, "status", || {
            formatted.set(true);
            "204".to_string()
        });
        assert!(formatted.get());
        assert_eq!(data.get("status").map(String::as_str), Some("204"));
    }

    #[test]
    fn undecodable_uri_fallback_is_sanitized_too() {
        let uri = "/bad%ZZ\\tail\r\n";
        assert_eq!(sanitize_request_uri(uri), "/bad%ZZ\\\\tail\\r\\n");
    }
}
