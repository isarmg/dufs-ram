//! Product HTML and response-body adaptation; no authentication policy or state.
use super::{Response, Server};
use crate::http_utils::body_full;
use anyhow::Result;
use headers::{ContentLength, ContentType, HeaderMapExt};
use http_body_util::{BodyExt, Limited};
use hyper::{
    HeaderMap, Method, StatusCode,
    header::{self, HeaderValue},
};

pub(super) const LOGIN_PATH: &str = "__dufs__/login";
const LOGIN_HTML: &str = include_str!("../../clients/web/login.html");

pub(super) async fn platform_response<B>(response: hyper::Response<B>) -> Result<Response>
where
    B: hyper::body::Body<Data = bytes::Bytes>,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let (parts, body) = response.into_parts();
    let bytes = Limited::new(body, 32 * 1024)
        .collect()
        .await
        .map_err(|_| anyhow::anyhow!("platform response body failed"))?
        .to_bytes();
    Ok(Response::from_parts(parts, body_full(bytes)))
}

impl Server {
    pub(super) fn send_login_page_for_get(&self, res: &mut Response) -> Result<()> {
        let output = LOGIN_HTML
            .replace("__ASSETS_PREFIX__", &self.content.assets_prefix)
            .replace(
                "__MIN_PASSWORD_BYTES__",
                &sarmg_admin_auth::PASSWORD_MIN_BYTES.to_string(),
            )
            .replace(
                "__MAX_PASSWORD_BYTES__",
                &sarmg_admin_auth::PASSWORD_MAX_BYTES.to_string(),
            );
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::TEXT_HTML_UTF_8));
        res.headers_mut()
            .typed_insert(ContentLength(output.len() as u64));
        *res.status_mut() = StatusCode::OK;
        *res.body_mut() = body_full(output);
        self.add_private_security_headers(res);
        Ok(())
    }

    pub(super) fn redirect_unauthenticated_page(
        &self,
        method: &Method,
        headers: &HeaderMap,
        api: bool,
        res: &mut Response,
    ) {
        if res.status() == StatusCode::UNAUTHORIZED
            && !api
            && matches!(*method, Method::GET | Method::HEAD)
            && accepts_html(headers)
        {
            *res.status_mut() = StatusCode::SEE_OTHER;
            *res.body_mut() = body_full("");
            res.headers_mut().remove(header::CONTENT_LENGTH);
            res.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_static("/__dufs__/login"),
            );
        }
    }

    // Outer product lifecycle errors use the shared envelope; Foundation authentication errors pass through unchanged.
    pub(super) fn render_administrator_auth_error(
        &self,
        res: &mut Response,
        status: StatusCode,
        code: &'static str,
        _message: &str,
        retryable: bool,
        retry_after: Option<u64>,
    ) -> Result<()> {
        let envelope = sarmg_contracts::ErrorEnvelope::with_code(
            sarmg_contracts::ErrorCode::new(code)?,
            "Request could not be completed",
        )
        .retryable(retryable);
        *res.status_mut() = status;
        *res.body_mut() = body_full(serde_json::to_vec(&envelope)?);
        res.headers_mut()
            .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JSON));
        if let Some(seconds) = retry_after {
            res.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(&seconds.to_string())?,
            );
        }
        self.add_private_security_headers(res);
        Ok(())
    }

    pub(super) fn add_private_security_headers(&self, res: &mut Response) {
        for (name, value) in [
            ("cache-control", "private, no-store"),
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
            ("referrer-policy", "no-referrer"),
            (
                "permissions-policy",
                "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
            ),
            (
                "content-security-policy",
                "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
            ),
        ] {
            res.headers_mut()
                .insert(name, HeaderValue::from_static(value));
        }
    }
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value.split(',').any(|item| {
                let mut fields = item.split(';');
                if !fields
                    .next()
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/html"))
                {
                    return false;
                }
                let mut quality = None;
                for field in fields {
                    let Some((name, value)) = field.split_once('=') else {
                        return false;
                    };
                    if name.trim().eq_ignore_ascii_case("q") {
                        if quality.is_some() {
                            return false;
                        }
                        let (whole, fraction) =
                            value.trim().split_once('.').unwrap_or((value.trim(), ""));
                        if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit())
                        {
                            return false;
                        }
                        quality = Some(match whole {
                            "0" => fraction.bytes().any(|byte| byte != b'0'),
                            "1" => fraction.bytes().all(|byte| byte == b'0'),
                            _ => false,
                        });
                    }
                }
                quality.unwrap_or(true)
            })
        })
}
