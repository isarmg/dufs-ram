use super::*;

impl StoreWorker {
    pub(super) fn save_upload_session(
        &mut self,
        proposed: &StoredUploadSession,
        ttl_ms: i64,
    ) -> Result<StoreUploadSession> {
        let now = now_ms()?;
        let expires_at = expiration_time(now, ttl_ms)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM upload_sessions
              WHERE state IN (?1, ?2, ?3) AND expires_at_ms <= ?4",
            params![UPLOAD_COMMITTED, UPLOAD_REJECTED, UPLOAD_UNKNOWN, now],
        )?;
        let existing = load_upload_session(&transaction, proposed.key)?;
        if !proposed.state.is_terminal() {
            let conflicting_stage: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM upload_sessions
                     WHERE stage_path = ?1
                        AND state IN (?2, ?3, ?4)
                        AND NOT (owner_digest = ?5 AND upload_id = ?6)
                 )",
                params![
                    proposed.stage_path.as_os_str().as_bytes(),
                    UPLOAD_RUNNING,
                    UPLOAD_COMMIT_STARTED,
                    UPLOAD_AWAITING_CONFIRMATION,
                    proposed.key.owner.as_slice(),
                    proposed.key.id.as_slice(),
                ],
                |row| row.get(0),
            )?;
            if conflicting_stage {
                transaction.commit()?;
                return Ok(StoreUploadSession::Conflict);
            }
        }
        let (result, stored) = match existing {
            None => {
                let global_count: i64 =
                    transaction
                        .query_row("SELECT COUNT(*) FROM upload_sessions", [], |row| row.get(0))?;
                let owner_count: i64 = transaction.query_row(
                    "SELECT COUNT(*) FROM upload_sessions WHERE owner_digest = ?1",
                    [proposed.key.owner.as_slice()],
                    |row| row.get(0),
                )?;
                if global_count >= self.limits.upload_capacity
                    || owner_count >= self.limits.upload_per_owner
                {
                    transaction.commit()?;
                    return Ok(StoreUploadSession::Full);
                }
                (StoreUploadSession::Inserted, proposed.clone())
            }
            Some(existing) => match merge_upload_session(&existing, proposed) {
                Some(stored) if stored == existing => (StoreUploadSession::Unchanged, stored),
                Some(stored) => (StoreUploadSession::Updated, stored),
                None => {
                    transaction.commit()?;
                    return Ok(StoreUploadSession::Conflict);
                }
            },
        };
        let stage_device = stored
            .stage_identity
            .map(|identity| identity.device.to_be_bytes());
        let stage_inode = stored
            .stage_identity
            .map(|identity| identity.inode.to_be_bytes());
        let target_revision = stored.target_revision;
        transaction.execute(
            "INSERT INTO upload_sessions(
                 owner_digest, upload_id, target_path, stage_path, upload_length,
                 durable_offset, state, stage_device_be, stage_inode_be,
                 target_revision, updated_at_ms, expires_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(owner_digest, upload_id) DO UPDATE SET
                 durable_offset = excluded.durable_offset,
                 state = excluded.state,
                 stage_device_be = excluded.stage_device_be,
                 stage_inode_be = excluded.stage_inode_be,
                 target_revision = excluded.target_revision,
                 updated_at_ms = excluded.updated_at_ms,
                 expires_at_ms = excluded.expires_at_ms",
            params![
                stored.key.owner.as_slice(),
                stored.key.id.as_slice(),
                stored.target_path.as_os_str().as_bytes(),
                stored.stage_path.as_os_str().as_bytes(),
                i64::try_from(stored.upload_length)?,
                i64::try_from(stored.durable_offset)?,
                stored.state as i64,
                stage_device.as_ref().map(<[u8; 8]>::as_slice),
                stage_inode.as_ref().map(<[u8; 8]>::as_slice),
                target_revision.as_ref().map(<[u8; 32]>::as_slice),
                now,
                expires_at,
            ],
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub(super) fn expired_upload_sessions(
        &mut self,
        limit: i64,
    ) -> Result<Vec<StoredUploadSession>> {
        let now = now_ms()?;
        query_upload_sessions(
            &self.connection,
            "SELECT owner_digest, upload_id, target_path, stage_path, upload_length,
                    durable_offset, state, stage_device_be, stage_inode_be, target_revision
               FROM upload_sessions
              WHERE expires_at_ms <= ?1 AND state != ?2
              ORDER BY expires_at_ms, owner_digest, upload_id
              LIMIT ?3",
            params![now, UPLOAD_COMMIT_STARTED, limit],
        )
    }

    pub(super) fn remove_upload_session_if_matches(
        &mut self,
        expected: &StoredUploadSession,
    ) -> Result<bool> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if load_upload_session(&transaction, expected.key)?.as_ref() != Some(expected) {
            transaction.commit()?;
            return Ok(false);
        }
        let removed = transaction.execute(
            "DELETE FROM upload_sessions WHERE owner_digest = ?1 AND upload_id = ?2",
            params![expected.key.owner.as_slice(), expected.key.id.as_slice()],
        )?;
        ensure!(removed == 1, "Upload removal lost its locked session row");
        transaction.commit()?;
        Ok(true)
    }

    pub(super) fn remove_expired_upload_session_if_matches(
        &mut self,
        expected: &StoredUploadSession,
    ) -> Result<bool> {
        let now = now_ms()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if load_upload_session(&transaction, expected.key)?.as_ref() != Some(expected) {
            transaction.commit()?;
            return Ok(false);
        }
        let removed = transaction.execute(
            "DELETE FROM upload_sessions
              WHERE owner_digest = ?1 AND upload_id = ?2 AND expires_at_ms <= ?3",
            params![
                expected.key.owner.as_slice(),
                expected.key.id.as_slice(),
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(removed == 1)
    }
}

fn merge_upload_session(
    existing: &StoredUploadSession,
    proposed: &StoredUploadSession,
) -> Option<StoredUploadSession> {
    if existing.key != proposed.key
        || existing.target_path != proposed.target_path
        || existing.stage_path != proposed.stage_path
        || existing.upload_length != proposed.upload_length
        || proposed.durable_offset < existing.durable_offset
        || existing
            .stage_identity
            .zip(proposed.stage_identity)
            .is_some_and(|(existing, proposed)| existing != proposed)
        || (existing.stage_identity.is_some() && proposed.stage_identity.is_none())
    {
        return None;
    }

    let transition_allowed = if existing.state == proposed.state {
        !existing.state.is_terminal()
            || existing.durable_offset == proposed.durable_offset
                && existing.stage_identity == proposed.stage_identity
    } else {
        matches!(
            (existing.state, proposed.state),
            (
                StoredUploadState::Running,
                StoredUploadState::CommitStarted
                    | StoredUploadState::Rejected
                    | StoredUploadState::Unknown
            ) | (
                StoredUploadState::CommitStarted,
                StoredUploadState::Committed
                    | StoredUploadState::Rejected
                    | StoredUploadState::Unknown
                    | StoredUploadState::AwaitingConfirmation
            ) | (
                StoredUploadState::AwaitingConfirmation,
                StoredUploadState::CommitStarted | StoredUploadState::Rejected
            )
        )
    };
    let revision_change_allowed = existing.target_revision == proposed.target_revision
        || existing.state == proposed.state && !existing.state.is_terminal()
        || matches!(existing.state, StoredUploadState::AwaitingConfirmation);
    if !transition_allowed || !revision_change_allowed {
        return None;
    }

    Some(proposed.clone())
}

struct UploadDatabaseRow {
    owner: Vec<u8>,
    id: Vec<u8>,
    target_path: Vec<u8>,
    stage_path: Vec<u8>,
    upload_length: i64,
    durable_offset: i64,
    state: i64,
    stage_device: Option<Vec<u8>>,
    stage_inode: Option<Vec<u8>>,
    target_revision: Option<Vec<u8>>,
}

fn read_upload_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UploadDatabaseRow> {
    Ok(UploadDatabaseRow {
        owner: row.get(0)?,
        id: row.get(1)?,
        target_path: row.get(2)?,
        stage_path: row.get(3)?,
        upload_length: row.get(4)?,
        durable_offset: row.get(5)?,
        state: row.get(6)?,
        stage_device: row.get(7)?,
        stage_inode: row.get(8)?,
        target_revision: row.get(9)?,
    })
}

fn decode_upload_row(row: UploadDatabaseRow) -> Result<StoredUploadSession> {
    let stage_identity = match (row.stage_device, row.stage_inode) {
        (None, None) => None,
        (Some(device), Some(inode)) => Some(StoredFileIdentity {
            device: u64::from_be_bytes(
                device
                    .try_into()
                    .map_err(|_| anyhow!("Upload stage device has an invalid length"))?,
            ),
            inode: u64::from_be_bytes(
                inode
                    .try_into()
                    .map_err(|_| anyhow!("Upload stage inode has an invalid length"))?,
            ),
        }),
        _ => bail!("Upload stage identity is incomplete"),
    };
    let session = StoredUploadSession {
        key: UploadSessionKey {
            owner: row
                .owner
                .try_into()
                .map_err(|_| anyhow!("Upload owner digest has an invalid length"))?,
            id: row
                .id
                .try_into()
                .map_err(|_| anyhow!("Upload id has an invalid length"))?,
        },
        target_path: PathBuf::from(OsString::from_vec(row.target_path)),
        stage_path: PathBuf::from(OsString::from_vec(row.stage_path)),
        upload_length: u64::try_from(row.upload_length)
            .context("Upload length in the state database is negative")?,
        durable_offset: u64::try_from(row.durable_offset)
            .context("Upload offset in the state database is negative")?,
        state: StoredUploadState::from_database(row.state)?,
        stage_identity,
        target_revision: row
            .target_revision
            .map(|revision| {
                revision
                    .try_into()
                    .map_err(|_| anyhow!("Upload target revision has an invalid length"))
            })
            .transpose()?,
    };
    session.validate()?;
    Ok(session)
}

pub(super) fn load_upload_session(
    connection: &Connection,
    key: UploadSessionKey,
) -> Result<Option<StoredUploadSession>> {
    connection
        .query_row(
            "SELECT owner_digest, upload_id, target_path, stage_path, upload_length,
                    durable_offset, state, stage_device_be, stage_inode_be, target_revision
               FROM upload_sessions
              WHERE owner_digest = ?1 AND upload_id = ?2",
            params![key.owner.as_slice(), key.id.as_slice()],
            read_upload_row,
        )
        .optional()?
        .map(decode_upload_row)
        .transpose()
}

fn query_upload_sessions<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    parameters: P,
) -> Result<Vec<StoredUploadSession>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(parameters, read_upload_row)?;
    rows.map(|row| decode_upload_row(row?)).collect()
}
