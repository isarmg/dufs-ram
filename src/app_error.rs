use hyper::StatusCode;
use std::fmt;

/// HTTP-facing error metadata kept separate from transport response building.
///
/// Handlers can migrate to this type incrementally; the outer service boundary
/// already uses it to keep public messages and diagnostic sources distinct.
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
}
