use crate::utils::encode_hex;

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(super) const UPLOAD_TEMP_PREFIX: &str = ".dufs-upload-";
pub(super) const UPLOAD_TEMP_SUFFIX: &str = ".part";
// Keep the private directory inside a quarantine-name shape understood by
// v0.48 and earlier. Those releases therefore hide it and never recurse into
// it even if an operator rolls the binary back; schema v5 independently
// prevents such a downgrade from opening the state database. Production
// quarantine names use UUID v4, so the nil UUID cannot collide with one.
pub(super) const UPLOAD_STAGE_DIRECTORY: &str =
    ".dufs-quarantine-00000000-0000-0000-0000-000000000000.hold";
pub(super) const UPLOAD_STATE_SUFFIX: &str = ".state";
pub(super) const UPLOAD_STATE_TEMP_SUFFIX: &str = ".tmp";
pub(super) const DELETE_TRASH_PREFIX: &str = ".dufs-upload-delete-";
pub(super) const DELETE_TRASH_SUFFIX: &str = ".trash";
pub(super) const QUARANTINE_PREFIX: &str = ".dufs-quarantine-";
pub(super) const QUARANTINE_SUFFIX: &str = ".hold";
const READINESS_TARGET_TAG: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

fn upload_stage_name(target_tag: &str, upload_id: Uuid) -> String {
    format!("{UPLOAD_TEMP_PREFIX}{target_tag}-{upload_id}{UPLOAD_TEMP_SUFFIX}")
}

pub(in crate::server) fn upload_readiness_probe_name(upload_id: Uuid) -> String {
    upload_stage_name(READINESS_TARGET_TAG, upload_id)
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

pub(in crate::server) fn legacy_upload_temp_path(path: &Path, upload_id: Uuid) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Upload target has no parent directory"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Upload target has no file name"))?;
    let digest = Sha256::digest(file_name.to_string_lossy().as_bytes());
    let target_tag = encode_hex(digest);
    Ok(parent.join(upload_stage_name(&target_tag, upload_id)))
}

pub(in crate::server) fn is_upload_temp_path(
    target: &Path,
    upload_id: Uuid,
    stage: &Path,
) -> Result<bool> {
    Ok(upload_temp_path(target, upload_id)? == stage
        || legacy_upload_temp_path(target, upload_id)? == stage)
}

pub(in crate::server) fn upload_stage_directory(path: &Path) -> Option<&Path> {
    let directory = path.parent()?;
    (directory.file_name()?.to_str()? == UPLOAD_STAGE_DIRECTORY).then_some(directory)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InternalEntryName {
    StageDirectory,
    Stage,
    State,
    StateTemp,
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
    if is_upload_state_temp_name(file_name) {
        return Some(InternalEntryName::StateTemp);
    }
    if file_name
        .strip_suffix(UPLOAD_STATE_SUFFIX)
        .is_some_and(is_upload_stage_name)
    {
        return Some(InternalEntryName::State);
    }
    is_upload_stage_name(file_name).then_some(InternalEntryName::Stage)
}

fn is_upload_stage_name(file_name: &str) -> bool {
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

fn is_upload_state_temp_name(file_name: &str) -> bool {
    let Some(value) = file_name.strip_suffix(UPLOAD_STATE_TEMP_SUFFIX) else {
        return false;
    };
    let Some(separator_index) = value.len().checked_sub(37) else {
        return false;
    };
    value.get(separator_index..separator_index + 1) == Some("-")
        && value
            .get(..separator_index)
            .and_then(|state_name| state_name.strip_suffix(UPLOAD_STATE_SUFFIX))
            .is_some_and(is_upload_stage_name)
        && value
            .get(separator_index + 1..)
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
    fn retired_state_names_remain_strictly_reserved() {
        let stage = format!(
            "{UPLOAD_TEMP_PREFIX}{}-{}{UPLOAD_TEMP_SUFFIX}",
            "a".repeat(64),
            Uuid::nil()
        );
        assert_eq!(
            classify_internal_name(&format!("{stage}{UPLOAD_STATE_SUFFIX}")),
            Some(InternalEntryName::State)
        );
        assert_eq!(
            classify_internal_name(&format!(
                "{stage}{UPLOAD_STATE_SUFFIX}-{}{UPLOAD_STATE_TEMP_SUFFIX}",
                Uuid::nil()
            )),
            Some(InternalEntryName::StateTemp)
        );
        assert_eq!(
            classify_internal_name(&format!(
                "{stage}{UPLOAD_STATE_SUFFIX}-invalid{UPLOAD_STATE_TEMP_SUFFIX}"
            )),
            None
        );
    }

    #[test]
    fn generated_stage_readiness_and_trash_names_are_reserved() {
        let upload_id = Uuid::new_v4();
        let stage_path = upload_temp_path(Path::new("file.bin"), upload_id).unwrap();
        assert_eq!(stage_path.parent(), Some(Path::new(UPLOAD_STAGE_DIRECTORY)));
        assert!(is_quarantine_name(UPLOAD_STAGE_DIRECTORY));
        assert_eq!(
            classify_internal_name(UPLOAD_STAGE_DIRECTORY),
            Some(InternalEntryName::StageDirectory)
        );
        let stage = stage_path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            classify_internal_name(stage),
            Some(InternalEntryName::Stage)
        );

        let readiness = upload_readiness_probe_name(upload_id);
        assert_eq!(
            classify_internal_name(&readiness),
            Some(InternalEntryName::Stage)
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
    fn current_and_legacy_stage_paths_are_exactly_bound_to_the_target() {
        let upload_id = Uuid::new_v4();
        let target = Path::new("folder/file.bin");
        let current = upload_temp_path(target, upload_id).unwrap();
        let legacy = legacy_upload_temp_path(target, upload_id).unwrap();

        assert!(is_upload_temp_path(target, upload_id, &current).unwrap());
        assert!(is_upload_temp_path(target, upload_id, &legacy).unwrap());
        assert_eq!(
            upload_stage_directory(&current),
            Some(Path::new("folder").join(UPLOAD_STAGE_DIRECTORY).as_path())
        );
        assert_eq!(upload_stage_directory(&legacy), None);
        assert!(!is_upload_temp_path(Path::new("folder/other.bin"), upload_id, &current).unwrap());
    }
}
