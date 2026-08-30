use crate::utils::encode_hex;

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(super) const UPLOAD_TEMP_PREFIX: &str = ".dufs-upload-";
pub(super) const UPLOAD_TEMP_SUFFIX: &str = ".part";
pub(super) const UPLOAD_STAGE_DIRECTORY: &str = ".dufs-upload-stages";
const READINESS_PREFIX: &str = ".dufs-readiness-";
const READINESS_SUFFIX: &str = ".probe";
pub(super) const DELETE_TRASH_PREFIX: &str = ".dufs-upload-delete-";
pub(super) const DELETE_TRASH_SUFFIX: &str = ".trash";
pub(super) const QUARANTINE_PREFIX: &str = ".dufs-quarantine-";
pub(super) const QUARANTINE_SUFFIX: &str = ".hold";
fn upload_stage_name(target_tag: &str, upload_id: Uuid) -> String {
    format!("{UPLOAD_TEMP_PREFIX}{target_tag}-{upload_id}{UPLOAD_TEMP_SUFFIX}")
}

pub(in crate::server) fn upload_readiness_probe_name(upload_id: Uuid) -> String {
    format!("{READINESS_PREFIX}{upload_id}{READINESS_SUFFIX}")
}

pub(in crate::server) fn delete_trash_name(trash_id: Uuid) -> String {
    format!("{DELETE_TRASH_PREFIX}{trash_id}{DELETE_TRASH_SUFFIX}")
}

pub(in crate::server) fn quarantine_name(quarantine_id: Uuid) -> String {
    format!("{QUARANTINE_PREFIX}{quarantine_id}{QUARANTINE_SUFFIX}")
}

pub(in crate::server) fn upload_temp_path(path: &Path, upload_id: Uuid) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Upload target has no parent directory"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Upload target has no file name"))?;
    let digest = Sha256::digest(file_name.to_string_lossy().as_bytes());
    let target_tag = encode_hex(digest);
    Ok(parent
        .join(UPLOAD_STAGE_DIRECTORY)
        .join(upload_stage_name(&target_tag, upload_id)))
}

pub(in crate::server) fn is_upload_temp_path(
    target: &Path,
    upload_id: Uuid,
    stage: &Path,
) -> Result<bool> {
    Ok(upload_temp_path(target, upload_id)? == stage)
}

pub(in crate::server) fn upload_stage_directory(path: &Path) -> Option<&Path> {
    let directory = path.parent()?;
    (directory.file_name()?.to_str()? == UPLOAD_STAGE_DIRECTORY).then_some(directory)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InternalEntryName {
    StageDirectory,
    Readiness,
    DeleteTrash,
    Quarantine,
}

pub(in crate::server) fn is_internal_name(file_name: &str) -> bool {
    classify_internal_name(file_name).is_some()
}

pub(super) fn classify_internal_name(file_name: &str) -> Option<InternalEntryName> {
    if file_name == UPLOAD_STAGE_DIRECTORY {
        return Some(InternalEntryName::StageDirectory);
    }
    if is_quarantine_name(file_name) {
        return Some(InternalEntryName::Quarantine);
    }
    if is_delete_trash_name(file_name) {
        return Some(InternalEntryName::DeleteTrash);
    }
    if is_readiness_name(file_name) {
        return Some(InternalEntryName::Readiness);
    }
    None
}

pub(super) fn is_upload_stage_name(file_name: &str) -> bool {
    let Some(value) = file_name
        .strip_prefix(UPLOAD_TEMP_PREFIX)
        .and_then(|value| value.strip_suffix(UPLOAD_TEMP_SUFFIX))
    else {
        return false;
    };
    let Some(target_tag) = value.get(..64) else {
        return false;
    };
    if !target_tag
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    value.get(64..65) == Some("-") && value.get(65..).is_some_and(is_canonical_uuid)
}

fn is_readiness_name(file_name: &str) -> bool {
    file_name
        .strip_prefix(READINESS_PREFIX)
        .and_then(|value| value.strip_suffix(READINESS_SUFFIX))
        .is_some_and(is_canonical_uuid)
}

fn is_delete_trash_name(file_name: &str) -> bool {
    file_name
        .strip_prefix(DELETE_TRASH_PREFIX)
        .and_then(|value| value.strip_suffix(DELETE_TRASH_SUFFIX))
        .is_some_and(is_canonical_uuid)
}

fn is_quarantine_name(file_name: &str) -> bool {
    file_name
        .strip_prefix(QUARANTINE_PREFIX)
        .and_then(|value| value.strip_suffix(QUARANTINE_SUFFIX))
        .is_some_and(is_canonical_uuid)
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|uuid| uuid.hyphenated().to_string() == value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn former_state_sidecar_names_are_not_reserved() {
        let stage = format!(
            "{UPLOAD_TEMP_PREFIX}{}-{}{UPLOAD_TEMP_SUFFIX}",
            "a".repeat(64),
            Uuid::nil()
        );
        assert_eq!(classify_internal_name(&format!("{stage}.state")), None);
        assert_eq!(
            classify_internal_name(&format!("{stage}.state-{}.tmp", Uuid::nil())),
            None
        );
    }

    #[test]
    fn generated_stage_readiness_and_trash_names_are_reserved() {
        let upload_id = Uuid::new_v4();
        let stage_path = upload_temp_path(Path::new("file.bin"), upload_id).unwrap();
        assert_eq!(stage_path.parent(), Some(Path::new(UPLOAD_STAGE_DIRECTORY)));
        assert_eq!(
            classify_internal_name(UPLOAD_STAGE_DIRECTORY),
            Some(InternalEntryName::StageDirectory)
        );
        let stage = stage_path.file_name().unwrap().to_str().unwrap();
        assert!(is_upload_stage_name(stage));
        assert_eq!(classify_internal_name(stage), None);

        let readiness = upload_readiness_probe_name(upload_id);
        assert_eq!(
            classify_internal_name(&readiness),
            Some(InternalEntryName::Readiness)
        );

        let trash = delete_trash_name(upload_id);
        assert_eq!(
            classify_internal_name(&trash),
            Some(InternalEntryName::DeleteTrash)
        );

        let quarantine = quarantine_name(upload_id);
        assert_eq!(
            classify_internal_name(&quarantine),
            Some(InternalEntryName::Quarantine)
        );
    }

    #[test]
    fn current_stage_paths_are_exactly_bound_to_the_target() {
        let upload_id = Uuid::new_v4();
        let target = Path::new("folder/file.bin");
        let current = upload_temp_path(target, upload_id).unwrap();

        assert!(is_upload_temp_path(target, upload_id, &current).unwrap());
        assert_eq!(
            upload_stage_directory(&current),
            Some(Path::new("folder").join(UPLOAD_STAGE_DIRECTORY).as_path())
        );
        assert!(!is_upload_temp_path(Path::new("folder/other.bin"), upload_id, &current).unwrap());
    }
}
