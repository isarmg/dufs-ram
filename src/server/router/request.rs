use super::super::{
    Request,
    browser_api::{BROWSER_API_PREFIX, is_tracked_browser_mutation},
    listing::LIST_API_PATH,
    operation_registry::{JOB_STATUS_PREFIX, parse_operation_id},
    session::LOGOUT_PATH,
    upload::{parse_upload_id, parse_upload_length, parse_upload_offset},
};

use hyper::Method;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UploadRequestContext {
    pub(super) id: Uuid,
    pub(super) length: u64,
    pub(super) offset: Option<u64>,
}

/// Facts derived at the HTTP boundary and reused by shutdown, timeout, error,
/// cache, and access-log policy. Keeping this classification in one value
/// prevents those branches from slowly acquiring different definitions of an
/// "internal" or "tracked" request.
pub(super) struct RequestProfile {
    public_asset: bool,
    omit_success_log: bool,
    upload: bool,
    internal_api: bool,
    upload_context: Option<UploadRequestContext>,
    operation_id: Option<Uuid>,
    mutation: MutationProgress,
}

impl RequestProfile {
    pub(super) fn new(req: &Request, relative_path: Option<&str>, public_asset: bool) -> Self {
        let method = req.method();
        let upload = matches!(method, &Method::PUT | &Method::PATCH);
        let upload_status =
            method == Method::HEAD && req.headers().contains_key("x-dufs-upload-id");
        let tracked_operation = method == Method::DELETE
            || (method == Method::POST && relative_path.is_some_and(is_tracked_browser_mutation));
        let internal_api = relative_path.is_some_and(|path| {
            path == LIST_API_PATH
                || path.starts_with(BROWSER_API_PREFIX)
                || path.starts_with(JOB_STATUS_PREFIX)
                || (path == LOGOUT_PATH && method == Method::POST)
                || upload
                || upload_status
                || method == Method::DELETE
        });
        let upload_context = upload
            .then(|| {
                match (
                    parse_upload_id(req.headers()).ok().flatten(),
                    parse_upload_length(req.headers()).ok().flatten(),
                    parse_upload_offset(req.headers()).ok().flatten(),
                ) {
                    (Some(id), Some(length), offset) => {
                        Some(UploadRequestContext { id, length, offset })
                    }
                    _ => None,
                }
            })
            .flatten();
        let operation_id = tracked_operation
            .then(|| parse_operation_id(req.headers()).ok().flatten())
            .flatten();

        Self {
            public_asset,
            omit_success_log: method == Method::GET && public_asset,
            upload,
            internal_api,
            upload_context,
            operation_id,
            mutation: MutationProgress::default(),
        }
    }

    pub(super) const fn is_public_asset(&self) -> bool {
        self.public_asset
    }

    pub(super) const fn omit_success_log(&self) -> bool {
        self.omit_success_log
    }

    pub(super) const fn is_upload(&self) -> bool {
        self.upload
    }

    pub(super) const fn is_internal_api(&self) -> bool {
        self.internal_api
    }

    pub(super) const fn upload_context(&self) -> Option<UploadRequestContext> {
        self.upload_context
    }

    pub(super) const fn operation_id(&self) -> Option<Uuid> {
        self.operation_id
    }

    pub(super) fn mutation(&self) -> MutationProgress {
        self.mutation.clone()
    }
}

/// Tracks the cancellation boundary for a mutation request. Tracked operations
/// become uncertain when their detached commit starts. Uploads atomically race
/// their first mutation against the HTTP deadline, so a timeout can only claim
/// `not-started` after it has closed that boundary.
#[derive(Clone, Debug, Default)]
pub(in crate::server) struct MutationProgress(Arc<AtomicU8>);

impl MutationProgress {
    const PREFLIGHT: u8 = 0;
    const RESERVED: u8 = 1;
    const DETACHED_COMMIT: u8 = 2;
    const CANCELLED_BEFORE_UPLOAD_MUTATION: u8 = 3;

    pub(in crate::server) fn mark_reserved(&self) {
        let _ = self.0.compare_exchange(
            Self::PREFLIGHT,
            Self::RESERVED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(in crate::server) fn mark_detached_commit(&self) {
        self.0.store(Self::DETACHED_COMMIT, Ordering::Release);
    }

    /// Atomically cross the upload mutation boundary. A concurrent request
    /// timeout may close that boundary first; callers must stop without
    /// mutating when this returns false.
    pub(in crate::server) fn begin_upload_mutation(&self) -> bool {
        match self.0.compare_exchange(
            Self::PREFLIGHT,
            Self::DETACHED_COMMIT,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(Self::DETACHED_COMMIT) => true,
            Err(Self::CANCELLED_BEFORE_UPLOAD_MUTATION | Self::RESERVED) => false,
            Err(_) => false,
        }
    }

    /// Close the upload mutation boundary before returning a definite timeout.
    /// If a task already crossed it, its outcome must remain unknown.
    pub(in crate::server) fn cancel_upload_before_mutation(&self) -> bool {
        self.0
            .compare_exchange(
                Self::PREFLIGHT,
                Self::CANCELLED_BEFORE_UPLOAD_MUTATION,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(in crate::server) fn outcome_can_be_unknown(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::DETACHED_COMMIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_detached_commit_has_unknown_timeout_semantics() {
        let progress = MutationProgress::default();
        assert!(!progress.outcome_can_be_unknown());
        progress.mark_reserved();
        assert!(!progress.outcome_can_be_unknown());
        progress.mark_detached_commit();
        assert!(progress.outcome_can_be_unknown());
    }

    #[test]
    fn upload_timeout_and_mutation_boundary_are_atomic() {
        let cancelled = MutationProgress::default();
        assert!(cancelled.cancel_upload_before_mutation());
        assert!(!cancelled.begin_upload_mutation());
        assert!(!cancelled.outcome_can_be_unknown());

        let mutating = MutationProgress::default();
        assert!(mutating.begin_upload_mutation());
        assert!(!mutating.cancel_upload_before_mutation());
        assert!(mutating.outcome_can_be_unknown());
        assert!(
            mutating.begin_upload_mutation(),
            "the boundary is idempotent"
        );
    }
}
