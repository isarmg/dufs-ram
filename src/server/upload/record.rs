use super::super::{
    identity::OwnerId,
    rooted_fs::{
        CreatedAncestors, DeleteIdentity, LegacyUploadStageMigration, PrivateUploadStageValidation,
        RootedFs, TrashPurgeProgress,
    },
    state_store::{
        RejectUploadSession, StateStore, StoreUploadSession, StoredFileIdentity,
        StoredUploadSession, StoredUploadState, UploadSessionKey,
    },
    storage::sync_file_to_storage,
};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    fmt,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    fs,
    io::{self, AsyncWriteExt},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UploadRecordState {
    Running,
    AwaitingConfirmation,
    Committed,
    Rejected,
    Unknown,
}

pub(super) enum UploadRecordLookup {
    NotSeen,
    ForeignOwner,
    Found(UploadCheckpoint),
}

#[derive(Debug)]
pub(super) enum UploadDiscardLookup {
    NotSeen,
    ForeignOwner,
    StateConflict(UploadCheckpoint),
    Rejected(RejectedUploadRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RejectedUploadRecord {
    pub(super) upload_length: u64,
    pub(super) durable_offset: u64,
    stage_path: PathBuf,
    stage_identity: Option<StoredFileIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StageCleanupOutcome {
    RemovedOrAbsent,
    ReplacementPreserved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UploadRecordStoreError {
    Conflict,
    Full,
}

/// Immutable identity and path metadata shared by every durable transition of
/// one upload. Keeping it together prevents checkpoint and terminal call sites
/// from accidentally swapping paths, owners, IDs, or declared lengths.
#[derive(Clone, Copy)]
pub(super) struct UploadRecordContext<'a> {
    owner_id: OwnerId,
    upload_id: Uuid,
    target_path: &'a Path,
    stage_path: &'a Path,
    upload_length: u64,
    target_revision: Option<[u8; 32]>,
    target_revision_bound: bool,
}

impl<'a> UploadRecordContext<'a> {
    pub(super) const fn new(
        owner_id: OwnerId,
        upload_id: Uuid,
        target_path: &'a Path,
        stage_path: &'a Path,
        upload_length: u64,
    ) -> Self {
        Self {
            owner_id,
            upload_id,
            target_path,
            stage_path,
            upload_length,
            target_revision: None,
            target_revision_bound: false,
        }
    }

    pub(super) const fn with_target_revision(mut self, target_revision: Option<[u8; 32]>) -> Self {
        self.target_revision = target_revision;
        self.target_revision_bound = true;
        self
    }
}

impl fmt::Display for UploadRecordStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conflict => "Upload session conflicts with the existing owner and ID record",
            Self::Full => "Upload session store is full",
        })
    }
}

impl std::error::Error for UploadRecordStoreError {}

/// SQLite-backed upload-session records.
///
/// Owner/ID uniqueness, transitions, capacity, and expiry have exactly one
/// file-backed authority.
#[derive(Clone)]
pub(in crate::server) struct UploadRecordStore {
    rooted_fs: RootedFs,
    state_store: StateStore,
    ttl: Duration,
}

impl UploadRecordStore {
    const STARTUP_RECONCILIATION_PAGE_SIZE: usize = 16;

    pub(in crate::server) fn new(
        rooted_fs: RootedFs,
        state_store: StateStore,
        ttl: Duration,
    ) -> Result<Self> {
        ensure!(
            ttl.as_millis() > 0,
            "Upload record TTL must be at least one millisecond"
        );
        Ok(Self {
            rooted_fs,
            state_store,
            ttl,
        })
    }

    /// Upgrade durable v0.48 stage bindings in bounded keyset pages before the
    /// server starts any listener or maintenance task. Filesystem movement is
    /// made durable before the exact SQLite row is changed, so a crash in
    /// between is recognized by inode on the next startup. A stale terminal
    /// identity never blocks a later active owner of the same physical name.
    pub(in crate::server) fn reconcile_stage_layouts(&self) -> Result<()> {
        let mut after = None;
        loop {
            let sessions = self
                .state_store
                .upload_sessions_page_blocking(after, Self::STARTUP_RECONCILIATION_PAGE_SIZE)?;
            if sessions.is_empty() {
                break;
            }
            after = sessions.last().map(|session| session.key);
            for session in sessions {
                self.reconcile_stage_layout(session)?;
            }
        }
        Ok(())
    }

    fn reconcile_stage_layout(&self, session: StoredUploadSession) -> Result<()> {
        let upload_id = Uuid::from_bytes(session.key.id);
        let target_path = self
            .rooted_fs
            .resolve_state_path(&session.target_path)
            .with_context(|| format!("Upload {upload_id} has an invalid target path"))?;
        let stored_stage = self
            .rooted_fs
            .resolve_state_path(&session.stage_path)
            .with_context(|| format!("Upload {upload_id} has an invalid stage path"))?;
        ensure!(
            self.rooted_fs.state_relative_path(&target_path)? == session.target_path
                && self.rooted_fs.state_relative_path(&stored_stage)? == session.stage_path,
            "Upload {upload_id} has a non-canonical rooted path"
        );

        let private_stage = super::upload_temp_path(&target_path, upload_id)?;
        let legacy_stage = super::legacy_upload_temp_path(&target_path, upload_id)?;
        ensure!(
            stored_stage == private_stage || stored_stage == legacy_stage,
            "Upload {upload_id} has a stage path that does not match its target"
        );

        let stage_owning = !session.state.is_terminal();

        // A row already bound to the private layout needs no rename, but its
        // confidentiality boundary and any live inode binding still require
        // validation. An unrelated stale legacy occupant is deliberately not
        // consulted here.
        if stored_stage == private_stage {
            let validation = self
                .rooted_fs
                .validate_private_upload_stage(&private_stage, session.stage_identity)
                .with_context(|| {
                    format!("Upload {upload_id} has an invalid private staging directory")
                })?;
            if validation == PrivateUploadStageValidation::IdentityMismatch && stage_owning {
                return Err(if session.stage_identity.is_none() {
                    anyhow!("Upload {upload_id} has no durable inode identity for its active stage")
                } else {
                    anyhow!("Upload {upload_id} staging identity does not match its active record")
                });
            }
            return Ok(());
        }

        let migration = self
            .rooted_fs
            .migrate_legacy_upload_stage(&legacy_stage, &private_stage, session.stage_identity)
            .with_context(|| {
                format!("Failed to isolate the staging file for upload {upload_id}")
            })?;

        if migration == LegacyUploadStageMigration::IdentityMismatch {
            if stage_owning {
                return Err(if session.stage_identity.is_none() {
                    anyhow!("Upload {upload_id} has no durable inode identity for its active stage")
                } else {
                    anyhow!("Upload {upload_id} staging identity does not match its active record")
                });
            }
            return Ok(());
        }
        if migration == LegacyUploadStageMigration::Missing && !stage_owning {
            return Ok(());
        }

        let private_relative = self.rooted_fs.state_relative_path(&private_stage)?;
        if session.stage_path != private_relative {
            ensure!(
                self.state_store
                    .replace_upload_stage_path_blocking(session, private_relative)?,
                "Upload {upload_id} changed while its stage path was being migrated"
            );
        }
        Ok(())
    }

    /// Look up the owner-scoped UUID in SQLite.
    pub(super) async fn lookup(
        &self,
        owner_id: OwnerId,
        upload_id: Uuid,
        target_path: &Path,
        stage_path: &Path,
    ) -> Result<UploadRecordLookup> {
        let key = upload_session_key(owner_id, upload_id);
        let (target_relative, _stage_relative) =
            self.validated_relative_paths(upload_id, target_path, stage_path)?;
        if let Some(session) = self.state_store.upload_session(key).await? {
            return self
                .classify_stored_session(upload_id, &target_relative, target_path, session)
                .await;
        }
        Ok(UploadRecordLookup::NotSeen)
    }

    /// Reopen the exact inode recorded by the durable checkpoint.
    ///
    /// `lookup` deliberately does not lend an fd across unrelated async
    /// preparation work. PATCH therefore reloads the row and validates the
    /// descriptor it will actually mutate, closing the lookup/open replacement
    /// window for owner-scoped upload sessions.
    pub(super) async fn open_resumable_stage(
        &self,
        record: UploadRecordContext<'_>,
        durable_offset: u64,
        expected_state: UploadRecordState,
    ) -> Result<Option<fs::File>> {
        let key = upload_session_key(record.owner_id, record.upload_id);
        let (target_relative, stage_relative) =
            self.validated_relative_paths(record.upload_id, record.target_path, record.stage_path)?;
        let Some(session) = self.state_store.upload_session(key).await? else {
            return Ok(None);
        };
        if session.target_path != target_relative
            || session.stage_path != stage_relative
            || session.upload_length != record.upload_length
            || session.durable_offset != durable_offset
            || session.state
                != match expected_state {
                    UploadRecordState::Running => StoredUploadState::Running,
                    UploadRecordState::AwaitingConfirmation => {
                        StoredUploadState::AwaitingConfirmation
                    }
                    _ => return Ok(None),
                }
        {
            return Ok(None);
        }
        let Some(expected_identity) = session.stage_identity else {
            return Ok(None);
        };
        let open = match expected_state {
            UploadRecordState::AwaitingConfirmation => {
                self.rooted_fs.open_read(record.stage_path).await
            }
            UploadRecordState::Running => self.rooted_fs.open_write(record.stage_path).await,
            _ => return Ok(None),
        };
        let file = match open {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::NotADirectory
                        | io::ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata().await?;
        if !metadata.file_type().is_file()
            || metadata.nlink() != 1
            || metadata.len() < durable_offset
            || metadata.dev() != expected_identity.device
            || metadata.ino() != expected_identity.inode
        {
            return Ok(None);
        }
        Ok(Some(file))
    }

    /// Flushes and fsyncs the exact staging descriptor before advertising a
    /// running offset in SQLite.
    pub(super) async fn persist_checkpoint(
        &self,
        file: &mut fs::File,
        record: UploadRecordContext<'_>,
        durable_offset: u64,
    ) -> Result<()> {
        self.persist_checkpoint_inner(file, record, durable_offset, false)
            .await
    }

    /// Persist the first checkpoint for a newly created stage. The directory
    /// entry must reach stable storage before SQLite advertises any resumable
    /// offset; syncing only the file contents does not make its name durable.
    pub(super) async fn persist_initial_checkpoint(
        &self,
        file: &mut fs::File,
        record: UploadRecordContext<'_>,
        durable_offset: u64,
    ) -> Result<()> {
        self.persist_checkpoint_inner(file, record, durable_offset, true)
            .await
    }

    async fn persist_checkpoint_inner(
        &self,
        file: &mut fs::File,
        record: UploadRecordContext<'_>,
        durable_offset: u64,
        sync_parent: bool,
    ) -> Result<()> {
        ensure!(
            durable_offset <= record.upload_length,
            "Upload checkpoint exceeds declared length"
        );
        let stage_identity = self
            .sync_and_identify_stage(file, record.stage_path, durable_offset)
            .await?;
        if sync_parent {
            self.rooted_fs.sync_parent(record.stage_path).await?;
        }
        self.persist_state(
            record,
            durable_offset,
            StoredUploadState::Running,
            Some(stage_identity),
        )
        .await
    }

    /// Records the non-cancellable publication boundary.
    pub(super) async fn persist_commit_started(
        &self,
        file: &mut fs::File,
        record: UploadRecordContext<'_>,
    ) -> Result<()> {
        let stage_identity = self
            .sync_and_identify_stage(file, record.stage_path, record.upload_length)
            .await?;
        self.persist_state(
            record,
            record.upload_length,
            StoredUploadState::CommitStarted,
            Some(stage_identity),
        )
        .await
    }

    /// Preserve a fully durable stage after an atomic destination CAS rejects
    /// publication. The owner can later confirm a newer target revision and
    /// publish the same inode without retransmitting its contents.
    pub(super) async fn persist_awaiting_confirmation(
        &self,
        file: &mut fs::File,
        record: UploadRecordContext<'_>,
    ) -> Result<()> {
        let stage_identity = self
            .sync_and_identify_stage(file, record.stage_path, record.upload_length)
            .await?;
        self.persist_state(
            record,
            record.upload_length,
            StoredUploadState::AwaitingConfirmation,
            Some(stage_identity),
        )
        .await
    }

    pub(super) async fn persist_terminal(
        &self,
        record: UploadRecordContext<'_>,
        durable_offset: u64,
        state: UploadRecordState,
    ) -> Result<()> {
        let state = match state {
            UploadRecordState::Committed => StoredUploadState::Committed,
            UploadRecordState::Rejected => StoredUploadState::Rejected,
            UploadRecordState::Running => {
                return Err(anyhow!("A terminal upload record cannot be running"));
            }
            UploadRecordState::AwaitingConfirmation => {
                return Err(anyhow!(
                    "A terminal upload record cannot await confirmation"
                ));
            }
            UploadRecordState::Unknown => {
                return Err(anyhow!("A terminal upload record cannot be unknown"));
            }
        };
        self.persist_state(record, durable_offset, state, None)
            .await
    }

    pub(super) async fn persist_unknown(
        &self,
        record: UploadRecordContext<'_>,
        durable_offset: u64,
    ) -> Result<()> {
        ensure!(
            durable_offset == record.upload_length,
            "An unknown upload publication must be fully durable"
        );
        self.persist_state(record, durable_offset, StoredUploadState::Unknown, None)
            .await
    }

    /// Atomically changes only the existing, exactly bound upload row from
    /// awaiting confirmation to rejected. The update never inserts or deletes
    /// a row and returns the retained stage identity for idempotent cleanup.
    pub(super) async fn reject_for_discard(
        &self,
        owner_id: OwnerId,
        upload_id: Uuid,
        target_path: &Path,
        stage_path: &Path,
    ) -> Result<UploadDiscardLookup> {
        let key = upload_session_key(owner_id, upload_id);
        let (target_relative, _stage_relative) =
            self.validated_relative_paths(upload_id, target_path, stage_path)?;
        let Some(existing) = self.state_store.upload_session(key).await? else {
            return Ok(UploadDiscardLookup::NotSeen);
        };
        let stored_target = self.rooted_fs.resolve_state_path(&existing.target_path)?;
        let stored_stage = self.rooted_fs.resolve_state_path(&existing.stage_path)?;
        if existing.target_path != target_relative
            || stored_target != target_path
            || !super::is_upload_temp_path(&stored_target, upload_id, &stored_stage)?
            || self.rooted_fs.state_relative_path(&stored_stage)? != existing.stage_path
        {
            return Ok(UploadDiscardLookup::ForeignOwner);
        }
        let result = self
            .state_store
            .reject_upload_session(key, target_relative, existing.stage_path, self.ttl)
            .await?;
        Ok(match result {
            RejectUploadSession::NotFound => UploadDiscardLookup::NotSeen,
            RejectUploadSession::BindingConflict => UploadDiscardLookup::ForeignOwner,
            RejectUploadSession::StateConflict(session) => {
                UploadDiscardLookup::StateConflict(upload_checkpoint(&session))
            }
            RejectUploadSession::Rejected(session) => {
                UploadDiscardLookup::Rejected(RejectedUploadRecord {
                    upload_length: session.upload_length,
                    durable_offset: session.durable_offset,
                    stage_path: stored_stage,
                    stage_identity: session.stage_identity,
                })
            }
        })
    }

    /// Idempotently removes the stage owned by a durable rejected session.
    ///
    /// The rejected row remains the authority for both retries and the inode
    /// identity check. A missing or mismatched row is never permission to
    /// unlink the shared stage pathname.
    pub(super) async fn cleanup_rejected_stage(&self, record: &RejectedUploadRecord) -> Result<()> {
        let Some(identity) = record.stage_identity else {
            // Legacy rejected rows may predate durable stage identities. No
            // pathname is safe to unlink without that capability.
            return Ok(());
        };
        match self
            .remove_file_if_identity(&record.stage_path, identity)
            .await?
        {
            StageCleanupOutcome::RemovedOrAbsent | StageCleanupOutcome::ReplacementPreserved => {
                self.remove_stage_directory_if_empty(&record.stage_path)
                    .await?;
                Ok(())
            }
        }
    }

    /// Removes storage artifacts first and the database ambiguity barrier
    /// last. A cancellation or database failure therefore cannot make an
    /// incompletely removed stage look like a fresh owner+UUID session.
    pub(super) async fn reset(
        &self,
        owner_id: OwnerId,
        upload_id: Uuid,
        target_path: &Path,
        stage_path: &Path,
    ) -> Result<()> {
        let key = upload_session_key(owner_id, upload_id);
        let (target_relative, stage_relative) =
            self.validated_relative_paths(upload_id, target_path, stage_path)?;
        let Some(session) = self.state_store.upload_session(key).await? else {
            // A caller that created a stage but failed before recording it must
            // use `discard_unrecorded_stage` with the still-open descriptor.
            // A missing owner-scoped row is never authority to unlink a shared
            // physical stage pathname.
            return Ok(());
        };
        if session.target_path != target_relative || session.stage_path != stage_relative {
            return Err(UploadRecordStoreError::Conflict.into());
        }
        if let Some(identity) = session.stage_identity
            && self.remove_file_if_identity(stage_path, identity).await?
                == StageCleanupOutcome::ReplacementPreserved
        {
            return Err(UploadRecordStoreError::Conflict.into());
        }
        if !self
            .state_store
            .remove_upload_session_if_matches(session)
            .await?
        {
            return Err(UploadRecordStoreError::Conflict.into());
        }
        self.remove_stage_directory_if_empty(stage_path).await?;
        Ok(())
    }

    /// Remove a stage created by this request when SQLite rejected the first
    /// checkpoint. The live descriptor supplies the ownership capability; a
    /// path-only cleanup would let another owner reuse the UUID and be deleted.
    pub(super) async fn discard_unrecorded_stage(
        &self,
        file: &fs::File,
        stage_path: &Path,
    ) -> Result<()> {
        let metadata = file.metadata().await?;
        ensure!(
            metadata.file_type().is_file() && metadata.nlink() == 1,
            "Unrecorded upload stage is not a private regular file"
        );
        match self
            .remove_file_if_identity(
                stage_path,
                StoredFileIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
            )
            .await?
        {
            StageCleanupOutcome::RemovedOrAbsent => {}
            StageCleanupOutcome::ReplacementPreserved => {
                return Err(UploadRecordStoreError::Conflict.into());
            }
        }
        self.remove_stage_directory_if_empty(stage_path).await?;
        Ok(())
    }

    pub(super) async fn reset_and_ancestors(
        &self,
        owner_id: OwnerId,
        upload_id: Uuid,
        target_path: &Path,
        stage_path: &Path,
        created_ancestors: Option<CreatedAncestors>,
    ) -> Result<()> {
        self.reset(owner_id, upload_id, target_path, stage_path)
            .await?;
        if let Some(created) = created_ancestors {
            self.rooted_fs.rollback_created_ancestors(created).await?;
        }
        Ok(())
    }

    async fn persist_state(
        &self,
        record: UploadRecordContext<'_>,
        durable_offset: u64,
        state: StoredUploadState,
        stage_identity: Option<StoredFileIdentity>,
    ) -> Result<()> {
        let key = upload_session_key(record.owner_id, record.upload_id);
        let (target_path, stage_relative) =
            self.validated_relative_paths(record.upload_id, record.target_path, record.stage_path)?;
        let existing = if stage_identity.is_none() || !record.target_revision_bound {
            self.state_store.upload_session(key).await?
        } else {
            None
        };
        let stage_identity =
            stage_identity.or_else(|| existing.as_ref().and_then(|session| session.stage_identity));
        let target_revision = if record.target_revision_bound {
            record.target_revision
        } else {
            existing.and_then(|session| session.target_revision)
        };
        let session = StoredUploadSession {
            key,
            target_path,
            stage_path: stage_relative,
            upload_length: record.upload_length,
            durable_offset,
            state,
            stage_identity,
            target_revision,
        };
        match self
            .state_store
            .save_upload_session(session, self.ttl)
            .await?
        {
            StoreUploadSession::Inserted
            | StoreUploadSession::Updated
            | StoreUploadSession::Unchanged => {}
            StoreUploadSession::Conflict => {
                return Err(UploadRecordStoreError::Conflict.into());
            }
            StoreUploadSession::Full => {
                return Err(UploadRecordStoreError::Full.into());
            }
        }

        Ok(())
    }

    fn validated_relative_paths(
        &self,
        upload_id: Uuid,
        target_path: &Path,
        stage_path: &Path,
    ) -> Result<(PathBuf, PathBuf)> {
        ensure!(
            super::is_upload_temp_path(target_path, upload_id, stage_path)?,
            "Upload staging path does not match its target and ID"
        );
        let target_relative = self.rooted_fs.state_relative_path(target_path)?;
        let stage_relative = self.rooted_fs.state_relative_path(stage_path)?;
        ensure!(
            self.rooted_fs.resolve_state_path(&target_relative)? == target_path,
            "Upload target path is not a canonical root-relative path"
        );
        ensure!(
            self.rooted_fs.resolve_state_path(&stage_relative)? == stage_path,
            "Upload staging path is not a canonical root-relative path"
        );
        Ok((target_relative, stage_relative))
    }

    async fn classify_stored_session(
        &self,
        upload_id: Uuid,
        expected_target: &Path,
        target_path: &Path,
        session: StoredUploadSession,
    ) -> Result<UploadRecordLookup> {
        // Treat paths loaded from SQLite as untrusted bytes. Resolution rejects
        // absolute paths, `..`, empty paths, and non-normal components.
        let stored_target = self.rooted_fs.resolve_state_path(&session.target_path)?;
        let stored_stage = self.rooted_fs.resolve_state_path(&session.stage_path)?;
        if session.target_path != expected_target
            || stored_target != self.rooted_fs.resolve_state_path(expected_target)?
            || stored_target != target_path
            || !super::is_upload_temp_path(&stored_target, upload_id, &stored_stage)?
            || self.rooted_fs.state_relative_path(&stored_stage)? != session.stage_path
        {
            return Ok(UploadRecordLookup::ForeignOwner);
        }

        if matches!(
            session.state,
            StoredUploadState::Running | StoredUploadState::AwaitingConfirmation
        ) && session.durable_offset < session.upload_length
            && !self
                .valid_resumable_stage(
                    &stored_stage,
                    session.durable_offset,
                    session.stage_identity,
                )
                .await?
        {
            return Ok(UploadRecordLookup::NotSeen);
        }
        // A full running row may have crossed the rename boundary already.
        // Never downgrade that ambiguity barrier based on the current stage
        // pathname, even if it is absent or now names a different inode.

        Ok(UploadRecordLookup::Found(UploadCheckpoint {
            upload_length: session.upload_length,
            durable_offset: session.durable_offset,
            target_revision: session.target_revision,
            state: match session.state {
                StoredUploadState::Committed => UploadRecordState::Committed,
                StoredUploadState::Rejected => UploadRecordState::Rejected,
                StoredUploadState::Running => UploadRecordState::Running,
                StoredUploadState::AwaitingConfirmation => UploadRecordState::AwaitingConfirmation,
                StoredUploadState::CommitStarted | StoredUploadState::Unknown => {
                    UploadRecordState::Unknown
                }
            },
        }))
    }

    async fn sync_and_identify_stage(
        &self,
        file: &mut fs::File,
        stage_path: &Path,
        durable_offset: u64,
    ) -> Result<StoredFileIdentity> {
        file.flush().await?;
        sync_file_to_storage(file).await?;
        let metadata = file.metadata().await?;
        ensure!(metadata.file_type().is_file(), "Upload stage is not a file");
        ensure!(
            metadata.nlink() == 1,
            "Upload stage has multiple hard links"
        );
        ensure!(
            metadata.len() >= durable_offset,
            "Upload stage is shorter than its durable checkpoint"
        );
        let path_metadata = self.rooted_fs.metadata_nofollow(stage_path).await?;
        ensure!(
            path_metadata.file_type().is_file()
                && path_metadata.nlink() == 1
                && path_metadata.dev() == metadata.dev()
                && path_metadata.ino() == metadata.ino(),
            "Upload stage descriptor no longer matches its rooted path"
        );
        Ok(StoredFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    async fn valid_resumable_stage(
        &self,
        stage_path: &Path,
        durable_offset: u64,
        expected_identity: Option<StoredFileIdentity>,
    ) -> Result<bool> {
        let Some(expected_identity) = expected_identity else {
            return Ok(false);
        };
        let file = match self.rooted_fs.open_write(stage_path).await {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::NotADirectory
                        | io::ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata().await?;
        Ok(metadata.file_type().is_file()
            && metadata.nlink() == 1
            && metadata.len() >= durable_offset
            && metadata.dev() == expected_identity.device
            && metadata.ino() == expected_identity.inode)
    }

    async fn remove_file_if_identity(
        &self,
        path: &Path,
        expected: StoredFileIdentity,
    ) -> Result<StageCleanupOutcome> {
        let Some(entry) = self.rooted_fs.capture_entry_for_purge(path, false).await? else {
            return Ok(StageCleanupOutcome::RemovedOrAbsent);
        };
        let actual = entry.identity();
        if actual
            != (DeleteIdentity {
                device: expected.device,
                inode: expected.inode,
                is_directory: false,
            })
        {
            return Ok(StageCleanupOutcome::ReplacementPreserved);
        }
        match entry
            .purge_slice(1, Duration::from_secs(1), CancellationToken::new())
            .await
        {
            Ok(TrashPurgeProgress::Complete) => Ok(StageCleanupOutcome::RemovedOrAbsent),
            Ok(TrashPurgeProgress::Pending(_)) => Err(anyhow!(
                "Upload cleanup did not finish its single-file purge"
            )),
            Err(error) => Err(error.into()),
        }
    }

    async fn remove_stage_directory_if_empty(&self, stage_path: &Path) -> Result<()> {
        if super::upload_stage_directory(stage_path).is_some() {
            self.rooted_fs
                .remove_empty_upload_stage_directory(stage_path)
                .await?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UploadCheckpoint {
    pub(super) upload_length: u64,
    pub(super) durable_offset: u64,
    pub(super) state: UploadRecordState,
    pub(super) target_revision: Option<[u8; 32]>,
}

fn upload_session_key(owner_id: OwnerId, upload_id: Uuid) -> UploadSessionKey {
    UploadSessionKey {
        owner: owner_id.into_bytes(),
        id: *upload_id.as_bytes(),
    }
}

fn upload_checkpoint(session: &StoredUploadSession) -> UploadCheckpoint {
    UploadCheckpoint {
        upload_length: session.upload_length,
        durable_offset: session.durable_offset,
        target_revision: session.target_revision,
        state: match session.state {
            StoredUploadState::Committed => UploadRecordState::Committed,
            StoredUploadState::Rejected => UploadRecordState::Rejected,
            StoredUploadState::Running => UploadRecordState::Running,
            StoredUploadState::AwaitingConfirmation => UploadRecordState::AwaitingConfirmation,
            StoredUploadState::CommitStarted | StoredUploadState::Unknown => {
                UploadRecordState::Unknown
            }
        },
    }
}

pub(super) async fn rollback_upload_ancestors(
    rooted_fs: &RootedFs,
    created_ancestors: &mut Option<CreatedAncestors>,
) -> Result<()> {
    if let Some(created) = created_ancestors.take() {
        rooted_fs.rollback_created_ancestors(created).await?;
    }
    Ok(())
}

#[cfg(test)]
mod record_store_tests {
    use super::*;
    use futures_util::poll;
    use std::{os::unix::fs::PermissionsExt, task::Poll};

    const TEST_TTL: Duration = Duration::from_secs(60);

    fn temporary_store() -> StateStore {
        StateStore::temporary_for_test(32, 16, TEST_TTL).unwrap()
    }

    fn upload_paths(root: &Path, name: &str, upload_id: Uuid) -> (PathBuf, PathBuf) {
        let target = root.join(name);
        let stage = super::super::upload_temp_path(&target, upload_id).unwrap();
        (target, stage)
    }

    fn stored_session(
        rooted_fs: &RootedFs,
        owner: [u8; 32],
        upload_id: Uuid,
        target: &Path,
        stage: &Path,
        state: StoredUploadState,
        stage_identity: Option<StoredFileIdentity>,
    ) -> StoredUploadSession {
        let upload_length = 8;
        StoredUploadSession {
            key: UploadSessionKey {
                owner,
                id: *upload_id.as_bytes(),
            },
            target_path: rooted_fs.state_relative_path(target).unwrap(),
            stage_path: rooted_fs.state_relative_path(stage).unwrap(),
            upload_length,
            durable_offset: if state.is_terminal()
                || state == StoredUploadState::CommitStarted
                || state == StoredUploadState::AwaitingConfirmation
            {
                upload_length
            } else {
                4
            },
            state,
            stage_identity,
            target_revision: None,
        }
    }

    async fn create_stage(rooted_fs: &RootedFs, stage: &Path, contents: &[u8]) -> fs::File {
        let (mut file, _) = rooted_fs.create_private_upload_stage(stage).await.unwrap();
        file.write_all(contents).await.unwrap();
        file
    }

    async fn create_awaiting_stage(
        records: &UploadRecordStore,
        rooted_fs: &RootedFs,
        owner: OwnerId,
        upload_id: Uuid,
        target: &Path,
        stage: &Path,
        contents: &[u8],
    ) -> fs::File {
        let mut file = create_stage(rooted_fs, stage, contents).await;
        let context =
            UploadRecordContext::new(owner, upload_id, target, stage, contents.len() as u64)
                .with_target_revision(Some([7; 32]));
        records
            .persist_initial_checkpoint(&mut file, context, contents.len() as u64)
            .await
            .unwrap();
        records
            .persist_commit_started(&mut file, context)
            .await
            .unwrap();
        records
            .persist_awaiting_confirmation(&mut file, context)
            .await
            .unwrap();
        file
    }

    async fn replace_stage_with_distinct_inode(
        rooted_fs: &RootedFs,
        stage: &Path,
        original: &fs::File,
        contents: &[u8],
    ) -> fs::File {
        // Keep the unlinked inode alive until its replacement exists. If the
        // last descriptor is dropped first, Linux may immediately recycle the
        // inode number and invalidate the identity-mismatch premise.
        std::fs::remove_file(stage).unwrap();
        let replacement = create_stage(rooted_fs, stage, contents).await;
        let original_metadata = original.metadata().await.unwrap();
        let replacement_metadata = replacement.metadata().await.unwrap();
        assert_ne!(
            (original_metadata.dev(), original_metadata.ino()),
            (replacement_metadata.dev(), replacement_metadata.ino()),
            "the test must keep the unlinked original alive until its path is replaced"
        );
        replacement
    }

    fn found(lookup: UploadRecordLookup) -> UploadCheckpoint {
        match lookup {
            UploadRecordLookup::Found(checkpoint) => checkpoint,
            UploadRecordLookup::NotSeen => panic!("upload record was not seen"),
            UploadRecordLookup::ForeignOwner => panic!("upload record path did not match"),
        }
    }

    #[tokio::test]
    async fn startup_moves_legacy_stage_into_private_directory_without_losing_metadata() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let owner = OwnerId::persistent("legacy-stage-owner");
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("legacy.bin");
        let legacy = super::super::legacy_upload_temp_path(&target, upload_id).unwrap();
        let private = super::super::upload_temp_path(&target, upload_id).unwrap();
        let (mut file, _) = rooted_fs.create_private_new(&legacy).await.unwrap();
        file.write_all(b"partial").await.unwrap();
        file.flush().await.unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o640)).unwrap();
        let metadata = file.metadata().await.unwrap();
        let session = StoredUploadSession {
            key: upload_session_key(owner, upload_id),
            target_path: rooted_fs.state_relative_path(&target).unwrap(),
            stage_path: rooted_fs.state_relative_path(&legacy).unwrap(),
            upload_length: 8,
            durable_offset: 7,
            state: StoredUploadState::Running,
            stage_identity: Some(StoredFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
            target_revision: Some([9; 32]),
        };
        assert_eq!(
            state_store
                .save_upload_session(session.clone(), TEST_TTL)
                .await
                .unwrap(),
            StoreUploadSession::Inserted
        );

        records.reconcile_stage_layouts().unwrap();

        assert!(!legacy.exists());
        assert_eq!(std::fs::read(&private).unwrap(), b"partial");
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o640,
            "migration must preserve metadata already copied from an overwrite target"
        );
        assert_eq!(
            std::fs::metadata(private.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let mut migrated = session;
        migrated.stage_path = rooted_fs.state_relative_path(&private).unwrap();
        assert_eq!(
            state_store.upload_session(migrated.key).await.unwrap(),
            Some(migrated)
        );
        drop(file);
    }

    #[tokio::test]
    async fn startup_refuses_an_unidentifiable_legacy_stage() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let owner = OwnerId::persistent("legacy-stage-without-identity");
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("unidentified.bin");
        let legacy = super::super::legacy_upload_temp_path(&target, upload_id).unwrap();
        std::fs::write(&legacy, b"do not adopt").unwrap();
        let session = StoredUploadSession {
            key: upload_session_key(owner, upload_id),
            target_path: rooted_fs.state_relative_path(&target).unwrap(),
            stage_path: rooted_fs.state_relative_path(&legacy).unwrap(),
            upload_length: 12,
            durable_offset: 0,
            state: StoredUploadState::Running,
            stage_identity: None,
            target_revision: None,
        };
        state_store
            .save_upload_session(session.clone(), TEST_TTL)
            .await
            .unwrap();

        let error = records
            .reconcile_stage_layouts()
            .expect_err("an occupant without a durable inode identity must block startup");
        assert!(format!("{error:#}").contains("no durable inode identity"));
        assert_eq!(std::fs::read(&legacy).unwrap(), b"do not adopt");
        assert_eq!(
            state_store.upload_session(session.key).await.unwrap(),
            Some(session)
        );
    }

    #[tokio::test]
    async fn startup_ignores_a_stale_legacy_occupant_for_a_private_record() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let owner = OwnerId::persistent("private-stage-owner");
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("private.bin");
        let private = super::super::upload_temp_path(&target, upload_id).unwrap();
        let legacy = super::super::legacy_upload_temp_path(&target, upload_id).unwrap();
        let file = create_stage(&rooted_fs, &private, b"private checkpoint").await;
        let metadata = file.metadata().await.unwrap();
        std::fs::write(&legacy, b"unrelated stale occupant").unwrap();
        let session = StoredUploadSession {
            key: upload_session_key(owner, upload_id),
            target_path: rooted_fs.state_relative_path(&target).unwrap(),
            stage_path: rooted_fs.state_relative_path(&private).unwrap(),
            upload_length: 32,
            durable_offset: 18,
            state: StoredUploadState::Running,
            stage_identity: Some(StoredFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
            target_revision: None,
        };
        state_store
            .save_upload_session(session.clone(), TEST_TTL)
            .await
            .unwrap();

        records.reconcile_stage_layouts().unwrap();

        assert_eq!(std::fs::read(&private).unwrap(), b"private checkpoint");
        assert_eq!(std::fs::read(&legacy).unwrap(), b"unrelated stale occupant");
        assert_eq!(
            state_store.upload_session(session.key).await.unwrap(),
            Some(session)
        );
    }

    #[tokio::test]
    async fn startup_refuses_a_traversable_directory_for_a_private_record() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("private-mode.bin");
        let private = super::super::upload_temp_path(&target, upload_id).unwrap();
        let file = create_stage(&rooted_fs, &private, b"private checkpoint").await;
        let metadata = file.metadata().await.unwrap();
        let session = stored_session(
            &rooted_fs,
            [4; 32],
            upload_id,
            &target,
            &private,
            StoredUploadState::Running,
            Some(StoredFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
        );
        state_store
            .save_upload_session(session, TEST_TTL)
            .await
            .unwrap();
        std::fs::set_permissions(
            private.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let error = records
            .reconcile_stage_layouts()
            .expect_err("a traversable staging directory must block startup");
        assert!(format!("{error:#}").contains("private identity check"));

        std::fs::set_permissions(
            private.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        records.reconcile_stage_layouts().unwrap();
    }

    #[tokio::test]
    async fn startup_refuses_a_substituted_private_stage_for_an_active_record() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("private-replacement.bin");
        let private = super::super::upload_temp_path(&target, upload_id).unwrap();
        let file = create_stage(&rooted_fs, &private, b"original checkpoint").await;
        let metadata = file.metadata().await.unwrap();
        let session = stored_session(
            &rooted_fs,
            [5; 32],
            upload_id,
            &target,
            &private,
            StoredUploadState::Running,
            Some(StoredFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
        );
        state_store
            .save_upload_session(session, TEST_TTL)
            .await
            .unwrap();
        let _replacement =
            replace_stage_with_distinct_inode(&rooted_fs, &private, &file, b"substitute").await;

        let error = records
            .reconcile_stage_layouts()
            .expect_err("a substituted active stage must block startup");
        assert!(format!("{error:#}").contains("staging identity does not match"));
    }

    #[tokio::test]
    async fn startup_migrates_the_active_owner_past_a_stale_terminal_binding() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("reused-name.bin");
        let legacy = super::super::legacy_upload_temp_path(&target, upload_id).unwrap();
        let private = super::super::upload_temp_path(&target, upload_id).unwrap();
        let (mut file, _) = rooted_fs.create_private_new(&legacy).await.unwrap();
        file.write_all(b"live").await.unwrap();
        file.flush().await.unwrap();
        let metadata = file.metadata().await.unwrap();
        let live_identity = StoredFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let stale = stored_session(
            &rooted_fs,
            [1; 32],
            upload_id,
            &target,
            &legacy,
            StoredUploadState::Committed,
            Some(StoredFileIdentity {
                device: live_identity.device,
                inode: live_identity.inode.wrapping_add(1),
            }),
        );
        let live = stored_session(
            &rooted_fs,
            [2; 32],
            upload_id,
            &target,
            &legacy,
            StoredUploadState::Running,
            Some(live_identity),
        );
        state_store
            .save_upload_session(stale.clone(), TEST_TTL)
            .await
            .unwrap();
        state_store
            .save_upload_session(live.clone(), TEST_TTL)
            .await
            .unwrap();

        records.reconcile_stage_layouts().unwrap();

        assert!(!legacy.exists());
        assert_eq!(std::fs::read(&private).unwrap(), b"live");
        assert_eq!(
            state_store.upload_session(stale.key).await.unwrap(),
            Some(stale)
        );
        let mut migrated_live = live;
        migrated_live.stage_path = rooted_fs.state_relative_path(&private).unwrap();
        assert_eq!(
            state_store.upload_session(migrated_live.key).await.unwrap(),
            Some(migrated_live)
        );
    }

    #[tokio::test]
    async fn startup_isolates_an_exact_rejected_legacy_stage() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let target = temp.path().join("rejected.bin");
        let legacy = super::super::legacy_upload_temp_path(&target, upload_id).unwrap();
        let private = super::super::upload_temp_path(&target, upload_id).unwrap();
        let (mut file, _) = rooted_fs.create_private_new(&legacy).await.unwrap();
        file.write_all(b"rejected staged data").await.unwrap();
        file.flush().await.unwrap();
        let metadata = file.metadata().await.unwrap();
        let session = stored_session(
            &rooted_fs,
            [3; 32],
            upload_id,
            &target,
            &legacy,
            StoredUploadState::Rejected,
            Some(StoredFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }),
        );
        state_store
            .save_upload_session(session.clone(), TEST_TTL)
            .await
            .unwrap();

        records.reconcile_stage_layouts().unwrap();

        assert!(!legacy.exists());
        assert_eq!(std::fs::read(&private).unwrap(), b"rejected staged data");
        let mut migrated = session;
        migrated.stage_path = rooted_fs.state_relative_path(&private).unwrap();
        assert_eq!(
            state_store.upload_session(migrated.key).await.unwrap(),
            Some(migrated)
        );
    }

    #[tokio::test]
    async fn sqlite_records_are_root_relative_and_bind_the_stage_inode() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let owner_id = OwnerId::persistent("persistent-owner");
        let (target, stage) = upload_paths(temp.path(), "persistent.bin", upload_id);
        let mut file = create_stage(&rooted_fs, &stage, b"part").await;

        records
            .persist_initial_checkpoint(
                &mut file,
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8)
                    .with_target_revision(Some([7; 32])),
                4,
            )
            .await
            .unwrap();

        let stored = state_store
            .upload_session(upload_session_key(owner_id, upload_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.target_path, Path::new("persistent.bin"));
        assert_eq!(
            stored.stage_path,
            rooted_fs.state_relative_path(&stage).unwrap()
        );
        assert!(stored.stage_identity.is_some());
        let checkpoint = found(
            records
                .lookup(owner_id, upload_id, &target, &stage)
                .await
                .unwrap(),
        );
        assert_eq!(checkpoint.durable_offset, 4);

        let replacement =
            replace_stage_with_distinct_inode(&rooted_fs, &stage, &file, b"part").await;
        drop(file);
        drop(replacement);
        assert!(matches!(
            records
                .lookup(owner_id, upload_id, &target, &stage)
                .await
                .unwrap(),
            UploadRecordLookup::NotSeen
        ));
    }

    #[tokio::test]
    async fn file_backed_sqlite_records_follow_the_same_state_machine() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records = UploadRecordStore::new(rooted_fs.clone(), state_store, TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let owner_id = OwnerId::persistent("ephemeral-owner");
        let (target, stage) = upload_paths(temp.path(), "ephemeral.bin", upload_id);
        let mut file = create_stage(&rooted_fs, &stage, b"part").await;

        records
            .persist_checkpoint(
                &mut file,
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8)
                    .with_target_revision(Some([7; 32])),
                4,
            )
            .await
            .unwrap();
        let checkpoint = found(
            records
                .lookup(owner_id, upload_id, &target, &stage)
                .await
                .unwrap(),
        );
        assert_eq!(checkpoint.state, UploadRecordState::Running);
        assert_eq!(checkpoint.durable_offset, 4);

        file.write_all(b"done").await.unwrap();
        records
            .persist_checkpoint(
                &mut file,
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8),
                8,
            )
            .await
            .unwrap();
        records
            .persist_commit_started(
                &mut file,
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8)
                    .with_target_revision(Some([7; 32])),
            )
            .await
            .unwrap();
        records
            .persist_awaiting_confirmation(
                &mut file,
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8)
                    .with_target_revision(Some([7; 32])),
            )
            .await
            .unwrap();
        let awaiting = found(
            records
                .lookup(owner_id, upload_id, &target, &stage)
                .await
                .unwrap(),
        );
        assert_eq!(awaiting.state, UploadRecordState::AwaitingConfirmation);
        assert_eq!(awaiting.target_revision, Some([7; 32]));
        drop(file);
        let mut file = records
            .open_resumable_stage(
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8),
                8,
                UploadRecordState::AwaitingConfirmation,
            )
            .await
            .unwrap()
            .expect("a retained full stage must reopen after confirmation");
        records
            .persist_commit_started(
                &mut file,
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8)
                    .with_target_revision(Some([8; 32])),
            )
            .await
            .unwrap();
        assert_eq!(
            found(
                records
                    .lookup(owner_id, upload_id, &target, &stage)
                    .await
                    .unwrap()
            )
            .state,
            UploadRecordState::Unknown
        );
        drop(file);
        std::fs::remove_file(&stage).unwrap();
        records
            .persist_terminal(
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8),
                8,
                UploadRecordState::Committed,
            )
            .await
            .unwrap();
        assert_eq!(
            found(
                records
                    .lookup(owner_id, upload_id, &target, &stage)
                    .await
                    .unwrap()
            )
            .state,
            UploadRecordState::Committed
        );
    }

    #[tokio::test]
    async fn owner_uuid_records_reject_a_different_root_relative_target() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), temporary_store(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let owner_id = OwnerId::persistent("path-owner");
        let (target, stage) = upload_paths(temp.path(), "first.bin", upload_id);
        let mut file = create_stage(&rooted_fs, &stage, b"part").await;
        records
            .persist_checkpoint(
                &mut file,
                UploadRecordContext::new(owner_id, upload_id, &target, &stage, 8),
                4,
            )
            .await
            .unwrap();

        let (other_target, other_stage) = upload_paths(temp.path(), "other.bin", upload_id);
        assert!(matches!(
            records
                .lookup(owner_id, upload_id, &other_target, &other_stage,)
                .await
                .unwrap(),
            UploadRecordLookup::ForeignOwner
        ));

        let mut other_file = create_stage(&rooted_fs, &other_stage, b"part").await;
        let error = records
            .persist_checkpoint(
                &mut other_file,
                UploadRecordContext::new(owner_id, upload_id, &other_target, &other_stage, 8),
                4,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<UploadRecordStoreError>(),
            Some(&UploadRecordStoreError::Conflict)
        );
    }

    #[tokio::test]
    async fn owner_scoped_reset_cannot_remove_another_owners_physical_stage() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let owner_a = OwnerId::persistent("owner-a");
        let owner_b = OwnerId::persistent("owner-b");
        let (target, stage) = upload_paths(temp.path(), "shared-target.bin", upload_id);
        let mut file = create_stage(&rooted_fs, &stage, b"owner-a").await;
        records
            .persist_checkpoint(
                &mut file,
                UploadRecordContext::new(owner_a, upload_id, &target, &stage, 16),
                7,
            )
            .await
            .unwrap();
        drop(file);

        records
            .reset(owner_b, upload_id, &target, &stage)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&stage).unwrap(), b"owner-a");
        assert!(
            state_store
                .upload_session(upload_session_key(owner_a, upload_id))
                .await
                .unwrap()
                .is_some()
        );

        let mut owner_b_file = rooted_fs.open_write(&stage).await.unwrap();
        let error = records
            .persist_checkpoint(
                &mut owner_b_file,
                UploadRecordContext::new(owner_b, upload_id, &target, &stage, 16),
                7,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<UploadRecordStoreError>(),
            Some(&UploadRecordStoreError::Conflict)
        );
    }

    #[tokio::test]
    async fn resume_reopens_only_the_inode_stored_in_sqlite() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), temporary_store(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let owner = OwnerId::persistent("resume-owner");
        let (target, stage) = upload_paths(temp.path(), "resume.bin", upload_id);
        let mut file = create_stage(&rooted_fs, &stage, b"part").await;
        records
            .persist_checkpoint(
                &mut file,
                UploadRecordContext::new(owner, upload_id, &target, &stage, 8),
                4,
            )
            .await
            .unwrap();
        let replacement =
            replace_stage_with_distinct_inode(&rooted_fs, &stage, &file, b"part").await;
        drop(file);
        drop(replacement);
        assert!(
            records
                .open_resumable_stage(
                    UploadRecordContext::new(owner, upload_id, &target, &stage, 8),
                    4,
                    UploadRecordState::Running,
                )
                .await
                .unwrap()
                .is_none(),
            "a same-length replacement inode must never be resumed"
        );
    }

    #[tokio::test]
    async fn descriptor_cleanup_preserves_a_replacement_path() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), temporary_store(), TEST_TTL).unwrap();
        let upload_id = Uuid::new_v4();
        let (_, stage) = upload_paths(temp.path(), "discard.bin", upload_id);
        let original = create_stage(&rooted_fs, &stage, b"original").await;
        let parked = temp.path().join("parked-stage");
        std::fs::rename(&stage, &parked).unwrap();
        drop(create_stage(&rooted_fs, &stage, b"replacement").await);

        let error = records
            .discard_unrecorded_stage(&original, &stage)
            .await
            .expect_err("descriptor cleanup must reject a replacement inode");
        assert_eq!(
            error.downcast_ref::<UploadRecordStoreError>(),
            Some(&UploadRecordStoreError::Conflict)
        );
        assert_eq!(std::fs::read(&stage).unwrap(), b"replacement");
        assert_eq!(std::fs::read(&parked).unwrap(), b"original");
    }

    #[tokio::test]
    async fn rejected_stage_cleanup_resumes_after_cancelled_state_delivery() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let owner = OwnerId::persistent("discard-cancellation-owner");
        let upload_id = Uuid::new_v4();
        let (target, stage) = upload_paths(temp.path(), "cancelled-discard.bin", upload_id);
        let file = create_awaiting_stage(
            &records,
            &rooted_fs,
            owner,
            upload_id,
            &target,
            &stage,
            b"complete",
        )
        .await;
        let before = state_store
            .upload_session(upload_session_key(owner, upload_id))
            .await
            .unwrap()
            .unwrap();
        let release_actor = state_store.block_actor_for_test().unwrap();
        // Enqueue the mutating command itself before cancelling its reply.
        // Polling `reject_for_discard` here would only enqueue that helper's
        // preliminary read, so dropping it could never exercise a committed
        // rejection whose delivery was cancelled.
        let mut reject = Box::pin(state_store.reject_upload_session(
            upload_session_key(owner, upload_id),
            rooted_fs.state_relative_path(&target).unwrap(),
            rooted_fs.state_relative_path(&stage).unwrap(),
            TEST_TTL,
        ));
        assert!(
            matches!(poll!(reject.as_mut()), Poll::Pending),
            "the reject command did not wait behind the blocked actor"
        );
        drop(reject);
        release_actor.send(()).unwrap();
        state_store.probe_readiness().await.unwrap();

        let after = state_store
            .upload_session(upload_session_key(owner, upload_id))
            .await
            .unwrap()
            .unwrap();
        let mut expected = before;
        expected.state = StoredUploadState::Rejected;
        assert_eq!(after, expected);
        assert!(stage.exists());

        state_store.set_query_only(true).await.unwrap();
        let record = match records
            .reject_for_discard(owner, upload_id, &target, &stage)
            .await
            .unwrap()
        {
            UploadDiscardLookup::Rejected(record) => record,
            _ => panic!("the cancelled rejection was not idempotently replayed"),
        };
        records.cleanup_rejected_stage(&record).await.unwrap();
        assert!(!stage.exists());
        state_store.set_query_only(false).await.unwrap();

        let record = match records
            .reject_for_discard(owner, upload_id, &target, &stage)
            .await
            .unwrap()
        {
            UploadDiscardLookup::Rejected(record) => record,
            _ => panic!("the completed discard was not idempotently replayed"),
        };
        records.cleanup_rejected_stage(&record).await.unwrap();
        drop(file);
    }

    #[tokio::test]
    async fn rejected_stage_cleanup_preserves_a_replacement_and_still_completes() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), temporary_store(), TEST_TTL).unwrap();
        let owner = OwnerId::persistent("discard-replacement-owner");
        let upload_id = Uuid::new_v4();
        let (target, stage) = upload_paths(temp.path(), "replaced-discard.bin", upload_id);
        let original = create_awaiting_stage(
            &records,
            &rooted_fs,
            owner,
            upload_id,
            &target,
            &stage,
            b"original",
        )
        .await;
        let record = match records
            .reject_for_discard(owner, upload_id, &target, &stage)
            .await
            .unwrap()
        {
            UploadDiscardLookup::Rejected(record) => record,
            _ => panic!("the awaiting upload was not rejected"),
        };
        let replacement =
            replace_stage_with_distinct_inode(&rooted_fs, &stage, &original, b"replacement").await;
        records.cleanup_rejected_stage(&record).await.unwrap();
        assert_eq!(std::fs::read(&stage).unwrap(), b"replacement");
        let retry = match records
            .reject_for_discard(owner, upload_id, &target, &stage)
            .await
            .unwrap()
        {
            UploadDiscardLookup::Rejected(record) => record,
            _ => panic!("the rejected upload was not idempotently replayed"),
        };
        records.cleanup_rejected_stage(&retry).await.unwrap();
        assert_eq!(std::fs::read(&stage).unwrap(), b"replacement");
        assert_eq!(
            found(
                records
                    .lookup(owner, upload_id, &target, &stage)
                    .await
                    .unwrap()
            )
            .state,
            UploadRecordState::Rejected
        );
        drop(original);
        drop(replacement);
    }

    #[tokio::test]
    async fn rejected_stage_transition_fails_closed_on_sqlite_write_failure() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let state_store = temporary_store();
        let records =
            UploadRecordStore::new(rooted_fs.clone(), state_store.clone(), TEST_TTL).unwrap();
        let owner = OwnerId::persistent("discard-database-owner");
        let upload_id = Uuid::new_v4();
        let (target, stage) = upload_paths(temp.path(), "failed-discard.bin", upload_id);
        let file = create_awaiting_stage(
            &records,
            &rooted_fs,
            owner,
            upload_id,
            &target,
            &stage,
            b"complete",
        )
        .await;

        state_store.set_query_only(true).await.unwrap();
        records
            .reject_for_discard(owner, upload_id, &target, &stage)
            .await
            .expect_err("query-only SQLite must reject the terminal transition");
        state_store.set_query_only(false).await.unwrap();
        assert_eq!(
            found(
                records
                    .lookup(owner, upload_id, &target, &stage)
                    .await
                    .unwrap()
            )
            .state,
            UploadRecordState::AwaitingConfirmation
        );
        assert_eq!(std::fs::read(&stage).unwrap(), b"complete");

        let record = match records
            .reject_for_discard(owner, upload_id, &target, &stage)
            .await
            .unwrap()
        {
            UploadDiscardLookup::Rejected(record) => record,
            _ => panic!("the retry did not reject the awaiting upload"),
        };
        records.cleanup_rejected_stage(&record).await.unwrap();
        assert!(!stage.exists());
        drop(file);
    }
}
