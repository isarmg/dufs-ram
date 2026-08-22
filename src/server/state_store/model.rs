use super::{
    MAX_ERROR_CODE_CHARS, PURGE_CLAIMED, PURGE_PREPARED, PURGE_READY, UPLOAD_AWAITING_CONFIRMATION,
    UPLOAD_COMMIT_STARTED, UPLOAD_COMMITTED, UPLOAD_REJECTED, UPLOAD_RUNNING, UPLOAD_UNKNOWN,
};
#[cfg(test)]
use super::{UNKNOWN_CODE, UNKNOWN_STATUS};
use anyhow::{Result, bail, ensure};
use std::{
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::server) struct RootIdentity {
    pub(in crate::server) device: u64,
    pub(in crate::server) inode: u64,
}

impl RootIdentity {
    #[cfg(test)]
    pub(in crate::server) fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::server) struct OperationKey {
    pub(in crate::server) owner: [u8; 32],
    pub(in crate::server) id: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i64)]
pub(in crate::server) enum StoredTerminalState {
    Succeeded = 0,
    Failed = 1,
    Unknown = 2,
}

impl StoredTerminalState {
    pub(super) fn from_database(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::Succeeded),
            1 => Ok(Self::Failed),
            2 => Ok(Self::Unknown),
            _ => bail!("Invalid terminal operation state in the state database: {value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::server) struct StoredOutcome {
    pub(in crate::server) status: u16,
    pub(in crate::server) state: StoredTerminalState,
    pub(in crate::server) code: Option<String>,
}

impl StoredOutcome {
    #[cfg(test)]
    pub(super) fn uncertain() -> Self {
        Self {
            status: UNKNOWN_STATUS,
            state: StoredTerminalState::Unknown,
            code: Some(UNKNOWN_CODE.to_string()),
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            (100..=599).contains(&self.status),
            "Stored operation status must be a valid HTTP status"
        );
        match self.state {
            StoredTerminalState::Succeeded => {
                ensure!(
                    (200..=299).contains(&self.status),
                    "A succeeded operation must have a successful HTTP status"
                );
                ensure!(
                    self.code.is_none(),
                    "A succeeded operation cannot have an error code"
                );
            }
            StoredTerminalState::Failed | StoredTerminalState::Unknown => {
                ensure!(
                    !(200..=299).contains(&self.status),
                    "A failed or unknown operation cannot have a successful HTTP status"
                );
                ensure!(
                    self.code.is_some(),
                    "A failed or unknown operation must have an error code"
                );
            }
        }
        if let Some(code) = &self.code {
            let len = code.chars().count();
            ensure!(
                (1..=MAX_ERROR_CODE_CHARS).contains(&len),
                "Stored operation error code is empty or too long"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::server) enum StoreBegin {
    Started { lease: [u8; 16] },
    Running,
    Replay(StoredOutcome),
    Conflict,
    Full,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::server) enum StoreStatus {
    Running,
    Completed(StoredOutcome),
    NotFound,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::server) struct UploadSessionKey {
    pub(in crate::server) owner: [u8; 32],
    pub(in crate::server) id: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::server) struct StoredFileIdentity {
    pub(in crate::server) device: u64,
    pub(in crate::server) inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i64)]
pub(in crate::server) enum StoredUploadState {
    Running = UPLOAD_RUNNING,
    CommitStarted = UPLOAD_COMMIT_STARTED,
    Committed = UPLOAD_COMMITTED,
    Rejected = UPLOAD_REJECTED,
    Unknown = UPLOAD_UNKNOWN,
    AwaitingConfirmation = UPLOAD_AWAITING_CONFIRMATION,
}

impl StoredUploadState {
    pub(super) fn from_database(value: i64) -> Result<Self> {
        match value {
            UPLOAD_RUNNING => Ok(Self::Running),
            UPLOAD_COMMIT_STARTED => Ok(Self::CommitStarted),
            UPLOAD_COMMITTED => Ok(Self::Committed),
            UPLOAD_REJECTED => Ok(Self::Rejected),
            UPLOAD_UNKNOWN => Ok(Self::Unknown),
            UPLOAD_AWAITING_CONFIRMATION => Ok(Self::AwaitingConfirmation),
            _ => bail!("Invalid upload session state in the state database: {value}"),
        }
    }

    pub(super) fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Rejected | Self::Unknown)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::server) struct StoredUploadSession {
    pub(in crate::server) key: UploadSessionKey,
    pub(in crate::server) target_path: PathBuf,
    pub(in crate::server) stage_path: PathBuf,
    pub(in crate::server) upload_length: u64,
    pub(in crate::server) durable_offset: u64,
    pub(in crate::server) state: StoredUploadState,
    pub(in crate::server) stage_identity: Option<StoredFileIdentity>,
    pub(in crate::server) target_revision: Option<[u8; 32]>,
}

impl StoredUploadSession {
    pub(super) fn validate(&self) -> Result<()> {
        validate_stored_path(&self.target_path, "Upload target")?;
        validate_stored_path(&self.stage_path, "Upload stage")?;
        ensure!(
            self.target_path != self.stage_path,
            "Upload target and stage paths must differ"
        );
        ensure!(
            self.durable_offset <= self.upload_length,
            "Upload durable offset exceeds its declared length"
        );
        ensure!(
            self.upload_length <= i64::MAX as u64,
            "Upload length cannot be represented by SQLite"
        );
        ensure!(
            self.durable_offset <= i64::MAX as u64,
            "Upload offset cannot be represented by SQLite"
        );
        if matches!(
            self.state,
            StoredUploadState::CommitStarted
                | StoredUploadState::Committed
                | StoredUploadState::AwaitingConfirmation
        ) {
            ensure!(
                self.durable_offset == self.upload_length,
                "A committing or committed upload must be fully durable"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::server) enum StoreUploadSession {
    Inserted,
    Updated,
    Unchanged,
    Conflict,
    Full,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::server) struct PurgeJobKey {
    pub(in crate::server) owner: [u8; 32],
    pub(in crate::server) id: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i64)]
pub(in crate::server) enum StoredPurgeState {
    Prepared = PURGE_PREPARED,
    Ready = PURGE_READY,
    Claimed = PURGE_CLAIMED,
}

impl StoredPurgeState {
    pub(super) fn from_database(value: i64) -> Result<Self> {
        match value {
            PURGE_PREPARED => Ok(Self::Prepared),
            PURGE_READY => Ok(Self::Ready),
            PURGE_CLAIMED => Ok(Self::Claimed),
            _ => bail!("Invalid purge job state in the state database: {value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::server) struct StoredPurgeJob {
    pub(in crate::server) key: PurgeJobKey,
    pub(in crate::server) target_path: PathBuf,
    pub(in crate::server) trash_path: PathBuf,
    pub(in crate::server) source_identity: StoredFileIdentity,
    pub(in crate::server) is_directory: bool,
    pub(in crate::server) state: StoredPurgeState,
    pub(in crate::server) attempts: u32,
}

impl StoredPurgeJob {
    pub(super) fn validate_new(&self) -> Result<()> {
        validate_stored_path(&self.target_path, "Purge target")?;
        validate_stored_path(&self.trash_path, "Purge trash")?;
        ensure!(
            self.target_path != self.trash_path,
            "Purge target and trash paths must differ"
        );
        ensure!(
            self.state == StoredPurgeState::Prepared,
            "A new purge job must start in the prepared state"
        );
        ensure!(self.attempts == 0, "A new purge job cannot have attempts");
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::server) enum StorePurgeJob {
    Inserted,
    Existing,
    Conflict,
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::server) struct StatePathCursor {
    pub(super) kind: i64,
    pub(super) owner: [u8; 32],
    pub(super) id: [u8; 16],
    pub(super) slot: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::server) struct StateBlockingPath {
    pub(in crate::server) path: PathBuf,
    /// A fresh PUT may replace the target of an idle Running upload without
    /// changing the meaning of that upload's sibling stage path. Every other
    /// durable path (including CommitStarted targets and purge intents) must
    /// continue to block an exact replacement.
    pub(in crate::server) allows_exact_replacement: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::server) struct StatePathPage {
    pub(in crate::server) paths: Vec<StateBlockingPath>,
    pub(in crate::server) next: Option<StatePathCursor>,
}

pub(super) fn validate_stored_path(path: &Path, description: &str) -> Result<()> {
    let bytes = path.as_os_str().as_bytes();
    ensure!(!bytes.is_empty(), "{description} path cannot be empty");
    ensure!(
        bytes.len() <= 65_536,
        "{description} path is too long for the state database"
    );
    ensure!(
        !bytes.contains(&0),
        "{description} path contains a NUL byte"
    );
    Ok(())
}
