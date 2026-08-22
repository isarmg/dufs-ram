use crate::utils::encode_hex;

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(super) const UPLOAD_TEMP_PREFIX: &str = ".dufs-upload-";
pub(super) const UPLOAD_TEMP_SUFFIX: &str = ".part";
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
    Ok(parent.join(upload_stage_name(&target_tag, upload_id)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InternalEntryName {
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
}
