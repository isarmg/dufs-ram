use std::{
    collections::HashMap,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{Local, SecondsFormat};
use hyper::{Method, Version, header::HeaderName};

use crate::{logger::BoundedLogLine, server::Request, utils::decode_uri};

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
    pub fn data(&self, req: &Request) -> HashMap<String, String> {
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
        "authorization" | "proxy-authorization" | "cookie" | "x-dufs-csrf-token"
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
    use hyper::Uri;

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
                "$http_x_dufs_csrf_token",
                "$http_X_DUFS_CSRF_TOKEN",
                "$http_X_DuFs_CsRf_ToKeN",
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
