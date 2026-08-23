use super::*;

impl StoreWorker {
    pub(super) fn begin_operation(
        &mut self,
        key: OperationKey,
        fingerprint: [u8; 32],
    ) -> Result<StoreBegin> {
        let now = self.now_ms()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        purge_expired(&transaction, now)?;

        if let Some(existing) = load_operation(&transaction, key)? {
            let result = if existing.fingerprint != fingerprint {
                StoreBegin::Conflict
            } else {
                match existing.state {
                    OPERATION_RESERVED | OPERATION_COMMIT_STARTED => StoreBegin::Running,
                    OPERATION_COMPLETED => StoreBegin::Replay(existing.into_outcome()?),
                    state => bail!("Invalid operation state in the state database: {state}"),
                }
            };
            transaction.commit()?;
            return Ok(result);
        }

        let global_count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))?;
        let owner_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM operations WHERE owner_digest = ?1",
            [key.owner.as_slice()],
            |row| row.get(0),
        )?;
        if global_count >= self.limits.capacity || owner_count >= self.limits.per_owner {
            transaction.commit()?;
            return Ok(StoreBegin::Full);
        }

        let lease = Uuid::new_v4().into_bytes();
        transaction.execute(
            "INSERT INTO operations(
                 owner_digest, operation_id, fingerprint, lease_token, state,
                 created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                key.owner.as_slice(),
                key.id.as_slice(),
                fingerprint.as_slice(),
                lease.as_slice(),
                OPERATION_RESERVED,
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(StoreBegin::Started { lease })
    }

    pub(super) fn operation_status(&mut self, key: OperationKey) -> Result<StoreStatus> {
        let now = self.now_ms()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        purge_expired(&transaction, now)?;
        let result = match load_operation(&transaction, key)? {
            Some(operation) => match operation.state {
                OPERATION_RESERVED | OPERATION_COMMIT_STARTED => StoreStatus::Running,
                OPERATION_COMPLETED => StoreStatus::Completed(operation.into_outcome()?),
                state => bail!("Invalid operation state in the state database: {state}"),
            },
            None => StoreStatus::NotFound,
        };
        transaction.commit()?;
        Ok(result)
    }

    pub(super) fn mark_operation_commit_started(
        &mut self,
        key: OperationKey,
        lease: [u8; 16],
    ) -> Result<bool> {
        let now = self.now_ms()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE operations SET state = ?1, updated_at_ms = ?2
              WHERE owner_digest = ?3
                AND operation_id = ?4
                AND lease_token = ?5
                AND state = ?6",
            params![
                OPERATION_COMMIT_STARTED,
                now,
                key.owner.as_slice(),
                key.id.as_slice(),
                lease.as_slice(),
                OPERATION_RESERVED,
            ],
        )?;
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub(super) fn complete_operation(
        &mut self,
        key: OperationKey,
        lease: [u8; 16],
        outcome: &StoredOutcome,
    ) -> Result<bool> {
        let now = self.now_ms()?;
        let expires_at = expiration_time(now, self.limits.ttl_ms)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE operations
                SET state = ?1,
                    terminal_state = ?2,
                    http_status = ?3,
                    error_code = ?4,
                    updated_at_ms = ?5,
                    expires_at_ms = ?6
              WHERE owner_digest = ?7
                AND operation_id = ?8
                AND lease_token = ?9
                AND state IN (?10, ?11)",
            params![
                OPERATION_COMPLETED,
                outcome.state as i64,
                i64::from(outcome.status),
                outcome.code.as_deref(),
                now,
                expires_at,
                key.owner.as_slice(),
                key.id.as_slice(),
                lease.as_slice(),
                OPERATION_RESERVED,
                OPERATION_COMMIT_STARTED,
            ],
        )?;
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub(super) fn abandon_operation(&mut self, key: OperationKey, lease: [u8; 16]) -> Result<()> {
        let now = self.now_ms()?;
        let expires_at = expiration_time(now, self.limits.ttl_ms)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = transaction.execute(
            "DELETE FROM operations
              WHERE owner_digest = ?1
                AND operation_id = ?2
                AND lease_token = ?3
                AND state = ?4",
            params![
                key.owner.as_slice(),
                key.id.as_slice(),
                lease.as_slice(),
                OPERATION_RESERVED,
            ],
        )?;
        if removed == 0 {
            transaction.execute(
                "UPDATE operations
                    SET state = ?1,
                        terminal_state = ?2,
                        http_status = ?3,
                        error_code = ?4,
                        updated_at_ms = ?5,
                        expires_at_ms = ?6
                  WHERE owner_digest = ?7
                    AND operation_id = ?8
                    AND lease_token = ?9
                    AND state = ?10",
                params![
                    OPERATION_COMPLETED,
                    StoredTerminalState::Unknown as i64,
                    i64::from(UNKNOWN_STATUS),
                    UNKNOWN_CODE,
                    now,
                    expires_at,
                    key.owner.as_slice(),
                    key.id.as_slice(),
                    lease.as_slice(),
                    OPERATION_COMMIT_STARTED,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

struct LoadedOperation {
    fingerprint: [u8; 32],
    state: i64,
    terminal_state: Option<i64>,
    http_status: Option<i64>,
    code: Option<String>,
}

impl LoadedOperation {
    fn into_outcome(self) -> Result<StoredOutcome> {
        let state = self
            .terminal_state
            .ok_or_else(|| anyhow!("Completed operation is missing its terminal state"))?;
        let status = self
            .http_status
            .ok_or_else(|| anyhow!("Completed operation is missing its HTTP status"))?;
        let status =
            u16::try_from(status).context("Completed operation contains an invalid HTTP status")?;
        let outcome = StoredOutcome {
            status,
            state: StoredTerminalState::from_database(state)?,
            code: self.code,
        };
        outcome.validate()?;
        Ok(outcome)
    }
}

fn load_operation(connection: &Connection, key: OperationKey) -> Result<Option<LoadedOperation>> {
    let row = connection
        .query_row(
            "SELECT fingerprint, state, terminal_state, http_status, error_code
               FROM operations
              WHERE owner_digest = ?1 AND operation_id = ?2",
            params![key.owner.as_slice(), key.id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    row.map(|(fingerprint, state, terminal_state, http_status, code)| {
        let fingerprint = <[u8; 32]>::try_from(fingerprint)
            .map_err(|_| anyhow!("Operation fingerprint has an invalid length"))?;
        Ok(LoadedOperation {
            fingerprint,
            state,
            terminal_state,
            http_status,
            code,
        })
    })
    .transpose()
}

pub(super) fn purge_expired(transaction: &Transaction<'_>, now: i64) -> Result<()> {
    transaction.execute(
        "DELETE FROM operations
          WHERE state = ?1 AND expires_at_ms <= ?2",
        params![OPERATION_COMPLETED, now],
    )?;
    Ok(())
}
