use super::model::validate_stored_path;
use super::*;

impl StoreWorker {
    pub(super) fn prepare_purge_job(&mut self, proposed: &StoredPurgeJob) -> Result<StorePurgeJob> {
        let now = now_ms()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_purge_job(&transaction, proposed.key)? {
            transaction.commit()?;
            return Ok(
                if existing.target_path == proposed.target_path
                    && existing.trash_path == proposed.trash_path
                    && existing.source_identity == proposed.source_identity
                    && existing.is_directory == proposed.is_directory
                {
                    StorePurgeJob::Existing
                } else {
                    StorePurgeJob::Conflict
                },
            );
        }
        let conflicting_path: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM purge_jobs WHERE trash_path = ?1)",
            [proposed.trash_path.as_os_str().as_bytes()],
            |row| row.get(0),
        )?;
        if conflicting_path {
            transaction.commit()?;
            return Ok(StorePurgeJob::Conflict);
        }
        let global_count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM purge_jobs", [], |row| row.get(0))?;
        let owner_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM purge_jobs WHERE owner_digest = ?1",
            [proposed.key.owner.as_slice()],
            |row| row.get(0),
        )?;
        if global_count >= self.limits.purge_capacity || owner_count >= self.limits.purge_per_owner
        {
            transaction.commit()?;
            return Ok(StorePurgeJob::Full);
        }
        transaction.execute(
            "INSERT INTO purge_jobs(
                 owner_digest, job_id, target_path, trash_path,
                 source_device_be, source_inode_be, is_directory, state, attempts,
                 next_attempt_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?9, ?9)",
            params![
                proposed.key.owner.as_slice(),
                proposed.key.id.as_slice(),
                proposed.target_path.as_os_str().as_bytes(),
                proposed.trash_path.as_os_str().as_bytes(),
                proposed.source_identity.device.to_be_bytes().as_slice(),
                proposed.source_identity.inode.to_be_bytes().as_slice(),
                i64::from(proposed.is_directory),
                PURGE_PREPARED,
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(StorePurgeJob::Inserted)
    }

    pub(super) fn prepared_purge_jobs(&mut self, limit: i64) -> Result<Vec<StoredPurgeJob>> {
        query_purge_jobs(
            &self.connection,
            "SELECT owner_digest, job_id, target_path, trash_path,
                    source_device_be, source_inode_be, trash_revision,
                    is_directory, state, attempts
               FROM purge_jobs
              WHERE state = ?1
              ORDER BY created_at_ms, owner_digest, job_id
              LIMIT ?2",
            params![PURGE_PREPARED, limit],
        )
    }

    pub(super) fn purge_jobs(&mut self, limit: i64) -> Result<Vec<StoredPurgeJob>> {
        query_purge_jobs(
            &self.connection,
            "SELECT owner_digest, job_id, target_path, trash_path,
                    source_device_be, source_inode_be, trash_revision,
                    is_directory, state, attempts
               FROM purge_jobs
              ORDER BY created_at_ms, owner_digest, job_id
              LIMIT ?1",
            [limit],
        )
    }

    pub(super) fn state_blocking_paths(
        &mut self,
        after: Option<StatePathCursor>,
        limit: i64,
    ) -> Result<StatePathPage> {
        query_state_blocking_paths(&self.connection, after, limit)
    }

    pub(super) fn mark_purge_job_ready(
        &mut self,
        key: PurgeJobKey,
        trash_revision: [u8; 32],
    ) -> Result<bool> {
        let now = now_ms()?;
        let changed = self.connection.execute(
            "UPDATE purge_jobs
                SET state = ?1, trash_revision = ?2,
                    next_attempt_at_ms = ?3, updated_at_ms = ?3
              WHERE owner_digest = ?4 AND job_id = ?5 AND state = ?6",
            params![
                PURGE_READY,
                trash_revision.as_slice(),
                now,
                key.owner.as_slice(),
                key.id.as_slice(),
                PURGE_PREPARED,
            ],
        )?;
        if changed == 1 {
            return Ok(true);
        }
        Ok(load_purge_job(&self.connection, key)?.is_some_and(|job| {
            job.state != StoredPurgeState::Prepared && job.trash_revision == Some(trash_revision)
        }))
    }

    pub(super) fn claim_due_purge_job(&mut self) -> Result<Option<StoredPurgeJob>> {
        let now = now_ms()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = transaction
            .query_row(
                "SELECT owner_digest, job_id
                   FROM purge_jobs
                  WHERE state = ?1 AND next_attempt_at_ms <= ?2
                  ORDER BY next_attempt_at_ms, created_at_ms, owner_digest, job_id
                  LIMIT 1",
                params![PURGE_READY, now],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
            .map(|(owner, id)| purge_key_from_database(owner, id))
            .transpose()?;
        let Some(key) = key else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE purge_jobs SET state = ?1, updated_at_ms = ?2
              WHERE owner_digest = ?3 AND job_id = ?4 AND state = ?5",
            params![
                PURGE_CLAIMED,
                now,
                key.owner.as_slice(),
                key.id.as_slice(),
                PURGE_READY,
            ],
        )?;
        ensure!(changed == 1, "A selected purge job could not be claimed");
        let job = load_purge_job(&transaction, key)?
            .ok_or_else(|| anyhow!("A claimed purge job disappeared"))?;
        transaction.commit()?;
        Ok(Some(job))
    }

    pub(super) fn retry_purge_job(&mut self, key: PurgeJobKey, delay_ms: i64) -> Result<bool> {
        let now = now_ms()?;
        let next_attempt = expiration_time(now, delay_ms)?;
        Ok(self.connection.execute(
            "UPDATE purge_jobs
                SET state = ?1,
                    attempts = MIN(attempts + 1, 4294967295),
                    next_attempt_at_ms = ?2,
                    updated_at_ms = ?3
              WHERE owner_digest = ?4 AND job_id = ?5 AND state = ?6",
            params![
                PURGE_READY,
                next_attempt,
                now,
                key.owner.as_slice(),
                key.id.as_slice(),
                PURGE_CLAIMED,
            ],
        )? == 1)
    }

    pub(super) fn complete_purge_job(&mut self, key: PurgeJobKey) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM purge_jobs
              WHERE owner_digest = ?1 AND job_id = ?2 AND state = ?3",
            params![key.owner.as_slice(), key.id.as_slice(), PURGE_CLAIMED],
        )? == 1)
    }

    pub(super) fn remove_purge_job(&mut self, key: PurgeJobKey) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM purge_jobs WHERE owner_digest = ?1 AND job_id = ?2",
            params![key.owner.as_slice(), key.id.as_slice()],
        )? == 1)
    }
}

struct StatePathDatabaseRow {
    kind: i64,
    owner: Vec<u8>,
    id: Vec<u8>,
    slot: i64,
    path: Vec<u8>,
    allows_exact_replacement: i64,
}

fn read_state_path_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StatePathDatabaseRow> {
    Ok(StatePathDatabaseRow {
        kind: row.get(0)?,
        owner: row.get(1)?,
        id: row.get(2)?,
        slot: row.get(3)?,
        path: row.get(4)?,
        allows_exact_replacement: row.get(5)?,
    })
}

fn decode_state_path_row(
    row: StatePathDatabaseRow,
) -> Result<(StatePathCursor, StateBlockingPath)> {
    ensure!(matches!(row.kind, 0 | 1), "State path kind is invalid");
    ensure!(matches!(row.slot, 0 | 1), "State path slot is invalid");
    ensure!(
        matches!(row.allows_exact_replacement, 0 | 1),
        "State path replacement policy is invalid"
    );
    let path = PathBuf::from(OsString::from_vec(row.path));
    validate_stored_path(&path, "State path")?;
    Ok((
        StatePathCursor {
            kind: row.kind,
            owner: row
                .owner
                .try_into()
                .map_err(|_| anyhow!("State path owner digest has an invalid length"))?,
            id: row
                .id
                .try_into()
                .map_err(|_| anyhow!("State path id has an invalid length"))?,
            slot: row.slot,
        },
        StateBlockingPath {
            path,
            allows_exact_replacement: row.allows_exact_replacement != 0,
        },
    ))
}

fn query_state_blocking_paths(
    connection: &Connection,
    after: Option<StatePathCursor>,
    limit: i64,
) -> Result<StatePathPage> {
    const FIRST_PAGE: &str = r#"
WITH state_paths(kind, owner_digest, item_id, slot, path, allows_exact_replacement) AS (
    SELECT 0, owner_digest, upload_id, 0, target_path, state = ?1
      FROM upload_sessions
     WHERE state IN (?1, ?2, ?3)
    UNION ALL
    SELECT 0, owner_digest, upload_id, 1, stage_path, 0
      FROM upload_sessions
     WHERE state IN (?1, ?2, ?3)
    UNION ALL
    SELECT 1, owner_digest, job_id, 0, target_path, 0
      FROM purge_jobs
     WHERE state = ?4
    UNION ALL
    SELECT 1, owner_digest, job_id, 1, trash_path, 0
      FROM purge_jobs
)
SELECT kind, owner_digest, item_id, slot, path, allows_exact_replacement
  FROM state_paths
 ORDER BY kind, owner_digest, item_id, slot
 LIMIT ?5
"#;
    const NEXT_PAGE: &str = r#"
WITH state_paths(kind, owner_digest, item_id, slot, path, allows_exact_replacement) AS (
    SELECT 0, owner_digest, upload_id, 0, target_path, state = ?1
      FROM upload_sessions
     WHERE state IN (?1, ?2, ?3)
    UNION ALL
    SELECT 0, owner_digest, upload_id, 1, stage_path, 0
      FROM upload_sessions
     WHERE state IN (?1, ?2, ?3)
    UNION ALL
    SELECT 1, owner_digest, job_id, 0, target_path, 0
      FROM purge_jobs
     WHERE state = ?4
    UNION ALL
    SELECT 1, owner_digest, job_id, 1, trash_path, 0
      FROM purge_jobs
)
SELECT kind, owner_digest, item_id, slot, path, allows_exact_replacement
  FROM state_paths
 WHERE (kind, owner_digest, item_id, slot) > (?5, ?6, ?7, ?8)
 ORDER BY kind, owner_digest, item_id, slot
 LIMIT ?9
"#;

    let mut statement = connection.prepare(if after.is_some() {
        NEXT_PAGE
    } else {
        FIRST_PAGE
    })?;
    let rows = match after {
        Some(after) => statement.query_map(
            params![
                UPLOAD_RUNNING,
                UPLOAD_COMMIT_STARTED,
                UPLOAD_AWAITING_CONFIRMATION,
                PURGE_PREPARED,
                after.kind,
                after.owner.as_slice(),
                after.id.as_slice(),
                after.slot,
                limit,
            ],
            read_state_path_row,
        )?,
        None => statement.query_map(
            params![
                UPLOAD_RUNNING,
                UPLOAD_COMMIT_STARTED,
                UPLOAD_AWAITING_CONFIRMATION,
                PURGE_PREPARED,
                limit
            ],
            read_state_path_row,
        )?,
    };
    let decoded = rows
        .map(|row| decode_state_path_row(row?))
        .collect::<Result<Vec<_>>>()?;
    let next = (decoded.len() == usize::try_from(limit)?)
        .then(|| decoded.last().map(|(cursor, _)| *cursor))
        .flatten();
    let paths = decoded.into_iter().map(|(_, path)| path).collect();
    Ok(StatePathPage { paths, next })
}

struct PurgeDatabaseRow {
    owner: Vec<u8>,
    id: Vec<u8>,
    target_path: Vec<u8>,
    trash_path: Vec<u8>,
    source_device: Vec<u8>,
    source_inode: Vec<u8>,
    trash_revision: Option<Vec<u8>>,
    is_directory: i64,
    state: i64,
    attempts: i64,
}

fn read_purge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PurgeDatabaseRow> {
    Ok(PurgeDatabaseRow {
        owner: row.get(0)?,
        id: row.get(1)?,
        target_path: row.get(2)?,
        trash_path: row.get(3)?,
        source_device: row.get(4)?,
        source_inode: row.get(5)?,
        trash_revision: row.get(6)?,
        is_directory: row.get(7)?,
        state: row.get(8)?,
        attempts: row.get(9)?,
    })
}

fn purge_key_from_database(owner: Vec<u8>, id: Vec<u8>) -> Result<PurgeJobKey> {
    Ok(PurgeJobKey {
        owner: owner
            .try_into()
            .map_err(|_| anyhow!("Purge owner digest has an invalid length"))?,
        id: id
            .try_into()
            .map_err(|_| anyhow!("Purge job id has an invalid length"))?,
    })
}

fn decode_purge_row(row: PurgeDatabaseRow) -> Result<StoredPurgeJob> {
    ensure!(
        matches!(row.is_directory, 0 | 1),
        "Purge directory flag is invalid"
    );
    let job = StoredPurgeJob {
        key: purge_key_from_database(row.owner, row.id)?,
        target_path: PathBuf::from(OsString::from_vec(row.target_path)),
        trash_path: PathBuf::from(OsString::from_vec(row.trash_path)),
        source_identity: StoredFileIdentity {
            device: u64::from_be_bytes(
                row.source_device
                    .try_into()
                    .map_err(|_| anyhow!("Purge source device has an invalid length"))?,
            ),
            inode: u64::from_be_bytes(
                row.source_inode
                    .try_into()
                    .map_err(|_| anyhow!("Purge source inode has an invalid length"))?,
            ),
        },
        trash_revision: row
            .trash_revision
            .map(|revision| {
                revision
                    .try_into()
                    .map_err(|_| anyhow!("Purge trash revision has an invalid length"))
            })
            .transpose()?,
        is_directory: row.is_directory == 1,
        state: StoredPurgeState::from_database(row.state)?,
        attempts: u32::try_from(row.attempts)
            .context("Purge attempts in the state database are invalid")?,
    };
    validate_stored_path(&job.target_path, "Purge target")?;
    validate_stored_path(&job.trash_path, "Purge trash")?;
    ensure!(
        job.target_path != job.trash_path,
        "Purge target and trash paths must differ"
    );
    Ok(job)
}

pub(super) fn load_purge_job(
    connection: &Connection,
    key: PurgeJobKey,
) -> Result<Option<StoredPurgeJob>> {
    connection
        .query_row(
            "SELECT owner_digest, job_id, target_path, trash_path,
                    source_device_be, source_inode_be, trash_revision,
                    is_directory, state, attempts
               FROM purge_jobs
              WHERE owner_digest = ?1 AND job_id = ?2",
            params![key.owner.as_slice(), key.id.as_slice()],
            read_purge_row,
        )
        .optional()?
        .map(decode_purge_row)
        .transpose()
}

fn query_purge_jobs<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    parameters: P,
) -> Result<Vec<StoredPurgeJob>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(parameters, read_purge_row)?;
    rows.map(|row| decode_purge_row(row?)).collect()
}
