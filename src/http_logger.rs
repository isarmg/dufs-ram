use std::{
    collections::HashMap,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{Local, SecondsFormat};
use hyper::header::HeaderName;

use crate::{logger::BoundedLogLine, server::Request, utils::decode_uri};

pub const DEFAULT_LOG_FORMAT: &str =
    r#"$time_iso8601 $log_level - $remote_addr "$request" $status"#;
const MAX_LOG_FORMAT_BYTES: usize = 4096;
const MAX_LOG_FORMAT_ELEMENTS: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub struct HttpLogger {
    elements: Vec<LogElement>,
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

impl HttpLogger {
    pub fn data(&self, req: &Request) -> HashMap<String, String> {
        let mut data = HashMap::default();
        for element in self.elements.iter() {
            match element {
                LogElement::Variable(name) => match name.as_str() {
                    "request" | "request_method" | "request_uri" => {
                        let uri = req.uri().to_string();
                        let decoded_uri = decode_uri(&uri)
                            .map(|s| sanitize_log_value(&s))
                            .unwrap_or_else(|| uri.clone());
                        data.entry("request".to_string())
                            .or_insert_with(|| format!("{} {decoded_uri}", req.method()));
                        data.entry("request_method".to_string())
                            .or_insert_with(|| req.method().to_string());
                        data.entry("request_uri".to_string())
                            .or_insert_with(|| decoded_uri);
                    }
                    _ => {}
                },
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
        if self
            .elements
            .iter()
            .any(|element| matches!(element, LogElement::Variable(name) if name == "remote_user"))
        {
            data.insert("remote_user".to_string(), sanitize_log_value(user));
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
        let now = Local::now();
        let time_local = now.to_rfc3339_opts(SecondsFormat::Secs, false);
        let time_iso8601 = now.to_rfc3339_opts(SecondsFormat::Secs, true);
        let msec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| format!("{:.3}", d.as_secs_f64()))
            .unwrap_or_default();
        let log_level = if err.is_some() { "ERROR" } else { "INFO" };

        let mut output = BoundedLogLine::new();
        for element in self.elements.iter() {
            match element {
                LogElement::Literal(value) => output.push_str(value.as_str()),
                LogElement::Variable(name) => {
                    let resolved = match name.as_str() {
                        "time_local" => Some(time_local.as_str()),
                        "time_iso8601" => Some(time_iso8601.as_str()),
                        "msec" => Some(msec.as_str()),
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
        Ok(Self { elements })
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
}
