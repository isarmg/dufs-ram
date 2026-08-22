use super::*;

#[derive(Clone, Copy, Debug)]
pub(in crate::server) struct UploadTargetInspection {
    pub(in crate::server) exists: bool,
    pub(in crate::server) replaceable: bool,
    pub(in crate::server) revision: Option<TargetRevision>,
}

pub(super) fn target_revision(
    owner_id: OwnerId,
    canonical_relative_path: &Path,
    identity: ReplacementTargetIdentity,
) -> Option<TargetRevision> {
    if !identity.exists() {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(b"dufs-upload-target-revision-v1\0");
    digest.update(owner_id.into_bytes());
    let path_bytes = canonical_relative_path.as_os_str().as_bytes();
    digest.update((path_bytes.len() as u64).to_be_bytes());
    digest.update(path_bytes);
    identity.update_revision_digest(&mut digest);
    Some(TargetRevision::from_bytes(digest.finalize().into()))
}

pub(super) fn inspect_target_identity(
    owner_id: OwnerId,
    canonical_relative_path: &Path,
    identity: ReplacementTargetIdentity,
) -> UploadTargetInspection {
    UploadTargetInspection {
        exists: identity.exists(),
        replaceable: identity.is_replaceable(),
        revision: target_revision(owner_id, canonical_relative_path, identity),
    }
}

pub(super) fn apply_target_inspection_headers(
    res: &mut Response,
    inspection: UploadTargetInspection,
) -> Result<()> {
    if let Some(revision) = inspection.revision {
        res.headers_mut().insert(
            TARGET_REVISION_HEADER,
            HeaderValue::from_str(&revision.encode())?,
        );
    } else {
        res.headers_mut().remove(TARGET_REVISION_HEADER);
    }
    res.headers_mut().insert(
        TARGET_REPLACEABLE_HEADER,
        HeaderValue::from_static(if inspection.replaceable {
            "true"
        } else {
            "false"
        }),
    );
    Ok(())
}

impl Server {
    pub(in crate::server) async fn inspect_upload_target(
        &self,
        owner: &str,
        path: &Path,
    ) -> Result<UploadTargetInspection> {
        let identity = self.content.rooted_fs.replacement_identity(path).await?;
        let canonical_relative_path = self.content.rooted_fs.state_relative_path(path)?;
        Ok(inspect_target_identity(
            OwnerId::persistent(owner),
            &canonical_relative_path,
            identity,
        ))
    }

    pub(super) async fn render_upload_target_conflict(
        &self,
        path: &Path,
        owner_id: OwnerId,
        context: UploadErrorContext,
        res: &mut Response,
    ) -> Result<()> {
        let canonical_relative_path = self.content.rooted_fs.state_relative_path(path)?;
        let inspection = inspect_target_identity(
            owner_id,
            &canonical_relative_path,
            self.content.rooted_fs.replacement_identity(path).await?,
        );
        apply_target_inspection_headers(res, inspection)?;
        let (code, detail) = if inspection.exists {
            (
                ErrorCode::DESTINATION_EXISTS,
                "The upload destination exists and requires explicit overwrite confirmation",
            )
        } else {
            (
                ErrorCode::UPLOAD_TARGET_CHANGED,
                "The upload destination changed after confirmation; refresh it before retrying",
            )
        };
        apply_upload_problem(
            res,
            context,
            StatusCode::CONFLICT,
            code,
            detail,
            RecoveryAdvice::RefreshTarget,
        )
    }
}
