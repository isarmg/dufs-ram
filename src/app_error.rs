use hyper::StatusCode;
use std::fmt;

/// HTTP-facing error metadata kept separate from transport response building.
///
/// The outer service boundary uses this type to keep public messages and
/// diagnostic sources distinct.
#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    public_message: String,
    source: Option<anyhow::Error>,
}

impl AppError {
    pub fn public(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            public_message: message.into(),
            source: None,
        }
    }

    pub fn internal(source: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            public_message: "Internal Server Error".to_string(),
            source: Some(source),
        }
    }

    /// Convert filesystem failures at the service boundary without exposing
    /// paths or kernel diagnostics to the client.
    pub fn from_anyhow(source: anyhow::Error) -> Self {
        let io_error = source
            .chain()
            .find_map(|error| error.downcast_ref::<std::io::Error>());
        let Some(io_error) = io_error else {
            return Self::internal(source);
        };
        let (status, public_message) = match io_error.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                (StatusCode::NOT_FOUND, "Not Found")
            }
            std::io::ErrorKind::PermissionDenied => (StatusCode::FORBIDDEN, "Forbidden"),
            std::io::ErrorKind::AlreadyExists => (StatusCode::CONFLICT, "Conflict"),
            std::io::ErrorKind::InvalidInput => (StatusCode::BAD_REQUEST, "Invalid request"),
            std::io::ErrorKind::TimedOut => (
                StatusCode::GATEWAY_TIMEOUT,
                "Filesystem operation timed out",
            ),
            std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => {
                (StatusCode::INSUFFICIENT_STORAGE, "Insufficient storage")
            }
            _ => return Self::internal(source),
        };
        Self {
            status,
            public_message: public_message.to_string(),
            source: Some(source),
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn public_message(&self) -> &str {
        &self.public_message
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(formatter, "{source:#}"),
            None => formatter.write_str(&self.public_message),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_error_keeps_diagnostics_out_of_the_public_message() {
        let error = AppError::internal(anyhow::anyhow!("private filesystem detail"));
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.public_message(), "Internal Server Error");
        assert!(error.to_string().contains("private filesystem detail"));
    }

    #[test]
    fn filesystem_errors_have_stable_http_semantics_without_leaking_details() {
        for (kind, status, message) in [
            (
                std::io::ErrorKind::NotFound,
                StatusCode::NOT_FOUND,
                "Not Found",
            ),
            (
                std::io::ErrorKind::PermissionDenied,
                StatusCode::FORBIDDEN,
                "Forbidden",
            ),
            (
                std::io::ErrorKind::AlreadyExists,
                StatusCode::CONFLICT,
                "Conflict",
            ),
            (
                std::io::ErrorKind::TimedOut,
                StatusCode::GATEWAY_TIMEOUT,
                "Filesystem operation timed out",
            ),
        ] {
            let error = AppError::from_anyhow(
                std::io::Error::new(kind, "private /srv/share detail").into(),
            );
            assert_eq!(error.status(), status);
            assert_eq!(error.public_message(), message);
            assert!(error.to_string().contains("/srv/share"));
            assert!(!error.public_message().contains("/srv/share"));
        }
    }
}
