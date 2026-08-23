use super::*;

pub(super) struct ActorRuntime {
    pub(super) path: PathBuf,
    pub(super) root: RootIdentity,
    pub(super) limits: Limits,
    pub(super) command_receiver: Receiver<Command>,
    pub(super) control_receiver: Receiver<ControlCommand>,
    pub(super) channels: ActorChannels,
    pub(super) healthy: Arc<AtomicBool>,
    pub(super) ready: SyncSender<Result<()>>,
}

struct HealthOnExit(Arc<AtomicBool>);

impl Drop for HealthOnExit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) fn run(runtime: ActorRuntime) {
    let ActorRuntime {
        path,
        root,
        limits,
        command_receiver,
        control_receiver,
        channels,
        healthy,
        ready,
    } = runtime;
    let _health_on_exit = HealthOnExit(healthy.clone());
    let connection = database::open_initialized_connection(&path, root, limits.ttl_ms)
        .with_context(|| "Failed to initialize the state database");

    let connection = match connection {
        Ok(connection) => connection,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    healthy.store(true, Ordering::Release);
    if ready.send(Ok(())).is_err() {
        return;
    }

    StoreWorker {
        connection,
        limits,
        deferred_abandons: VecDeque::new(),
    }
    .run(command_receiver, control_receiver, channels);
}

impl StoreWorker {
    fn run(
        mut self,
        command_receiver: Receiver<Command>,
        control_receiver: Receiver<ControlCommand>,
        channels: ActorChannels,
    ) {
        loop {
            if self.process_controls(&control_receiver) {
                break;
            }
            let command = match command_receiver.recv() {
                Ok(command) => command,
                Err(_) => break,
            };
            match command {
                Command::Wake => continue,
                Command::ProbeReadiness { reply } => {
                    // A failed probe describes this command, not the actor's
                    // lifecycle. A later probe may succeed after a transient
                    // lock, I/O, or capacity condition clears.
                    let _ = reply.send(self.probe_readiness());
                }
                Command::Begin {
                    key,
                    fingerprint,
                    reply,
                } => {
                    let result = self
                        .begin_operation(key, fingerprint)
                        .map(|begin| BeginEnvelope::new(begin, &channels, key));
                    match reply.send(result) {
                        Ok(()) | Err(Err(_)) => {}
                        Err(Ok(mut envelope)) => {
                            if let Some(lease) = envelope.reservation() {
                                // Cancellation before delivery leaves a
                                // Reserved row that must be removed. A single
                                // SQLite failure must not kill the actor, so a
                                // failed cleanup is retained for a later
                                // command boundary.
                                envelope.disarm_cleanup();
                                if let Err(error) = self.abandon_operation(key, lease) {
                                    log::error!(
                                        "State store reservation cleanup was deferred: {error:#}"
                                    );
                                    self.defer_abandon(key, lease);
                                }
                            }
                        }
                    }
                }
                Command::Status { key, reply } => {
                    let _ = reply.send(self.operation_status(key));
                }
                Command::MarkCommitStarted { key, lease, reply } => {
                    let _ = reply.send(self.mark_operation_commit_started(key, lease));
                }
                Command::Complete {
                    key,
                    lease,
                    outcome,
                    reply,
                } => {
                    let _ = reply.send(self.complete_operation(key, lease, &outcome));
                }
                Command::SaveUploadSession {
                    session,
                    ttl_ms,
                    reply,
                } => {
                    let _ = reply.send(self.save_upload_session(&session, ttl_ms));
                }
                Command::LoadUploadSession { key, reply } => {
                    let _ = reply.send(super::upload::load_upload_session(&self.connection, key));
                }
                Command::ListUploadSessionsPageBlocking {
                    after,
                    limit,
                    reply,
                } => {
                    let _ = reply.send(self.upload_sessions_page(after, limit));
                }
                Command::ListUploadSessionsPage {
                    after,
                    limit,
                    reply,
                } => {
                    let _ = reply.send(self.upload_sessions_page(after, limit));
                }
                Command::ReplaceUploadStagePathBlocking {
                    expected,
                    replacement,
                    reply,
                } => {
                    let _ = reply
                        .send(self.replace_upload_stage_path_if_matches(&expected, &replacement));
                }
                Command::RejectUploadSession {
                    key,
                    target_path,
                    stage_path,
                    ttl_ms,
                    reply,
                } => {
                    let _ = reply.send(self.reject_upload_session(
                        key,
                        &target_path,
                        &stage_path,
                        ttl_ms,
                    ));
                }
                Command::ListExpiredUploadSessions { limit, reply } => {
                    let _ = reply.send(self.expired_upload_sessions(limit));
                }
                Command::RemoveUploadSessionIfMatches { expected, reply } => {
                    let _ = reply.send(self.remove_upload_session_if_matches(&expected));
                }
                Command::RemoveExpiredUploadSessionIfMatches { expected, reply } => {
                    let _ = reply.send(self.remove_expired_upload_session_if_matches(&expected));
                }
                Command::PreparePurgeJob { job, reply } => {
                    let _ = reply.send(self.prepare_purge_job(&job));
                }
                Command::LoadPurgeJob { key, reply } => {
                    let _ = reply.send(super::purge::load_purge_job(&self.connection, key));
                }
                Command::ListPreparedPurgeJobs { limit, reply } => {
                    let _ = reply.send(self.prepared_purge_jobs(limit));
                }
                Command::ListPurgeJobs { limit, reply } => {
                    let _ = reply.send(self.purge_jobs(limit));
                }
                Command::ListStateBlockingPaths {
                    after,
                    limit,
                    reply,
                } => {
                    let _ = reply.send(self.state_blocking_paths(after, limit));
                }
                Command::MarkPurgeJobReady {
                    key,
                    trash_revision,
                    reply,
                } => {
                    let _ = reply.send(self.mark_purge_job_ready(key, trash_revision));
                }
                Command::ClaimDuePurgeJob { reply } => {
                    let _ = reply.send(self.claim_due_purge_job());
                }
                Command::RetryPurgeJob {
                    key,
                    delay_ms,
                    reply,
                } => {
                    let _ = reply.send(self.retry_purge_job(key, delay_ms));
                }
                Command::CompletePurgeJob { key, reply } => {
                    let _ = reply.send(self.complete_purge_job(key));
                }
                Command::RemovePurgeJob { key, reply } => {
                    let _ = reply.send(self.remove_purge_job(key));
                }
                #[cfg(test)]
                Command::InspectPragmas { reply } => {
                    let _ = reply.send(self.inspect_pragmas());
                }
                #[cfg(test)]
                Command::InjectSqlError { reply } => {
                    let result = self
                        .connection
                        .execute_batch("SELECT * FROM __dufs_missing_test_table")
                        .context("Injected state-store SQL command failed");
                    let _ = reply.send(result);
                }
                #[cfg(test)]
                Command::SetQueryOnly { enabled, reply } => {
                    let result = self
                        .connection
                        .pragma_update(None, "query_only", enabled)
                        .context("Failed to change SQLite query-only test mode");
                    let _ = reply.send(result);
                }
                #[cfg(test)]
                Command::Block { entered, release } => {
                    let _ = entered.send(());
                    let _ = release.recv();
                }
            }
        }
    }

    fn process_controls(&mut self, receiver: &Receiver<ControlCommand>) -> bool {
        self.retry_one_deferred_abandon();
        while let Ok(control) = receiver.try_recv() {
            match control {
                ControlCommand::Abandon { key, lease } => {
                    if let Err(error) = self.abandon_operation(key, lease) {
                        log::error!("State store control cleanup was deferred: {error:#}");
                        self.defer_abandon(key, lease);
                    }
                }
                ControlCommand::Shutdown => return true,
            }
        }
        false
    }

    fn defer_abandon(&mut self, key: OperationKey, lease: [u8; 16]) {
        if !self
            .deferred_abandons
            .iter()
            .any(|pending| *pending == (key, lease))
        {
            self.deferred_abandons.push_back((key, lease));
        }
    }

    fn retry_one_deferred_abandon(&mut self) {
        let Some((key, lease)) = self.deferred_abandons.pop_front() else {
            return;
        };
        if let Err(error) = self.abandon_operation(key, lease) {
            log::error!("State store deferred cleanup still cannot run: {error:#}");
            self.deferred_abandons.push_back((key, lease));
        }
    }

    fn probe_readiness(&mut self) -> Result<()> {
        let application_id: i32 = self
            .connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .context("Failed to read the state database identity")?;
        ensure!(
            application_id == APPLICATION_ID,
            "The state database application id changed"
        );

        // BEGIN IMMEDIATE plus a rolled-back row mutation checks both reads
        // and the actual rollback-journal write path without publishing probe
        // state. Merely consulting the cached `healthy` flag would miss a
        // database that became read-only or unavailable after startup.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("Failed to begin the state database readiness write probe")?;
        let metadata_rows: i64 = transaction
            .query_row("SELECT COUNT(*) FROM store_meta", [], |row| row.get(0))
            .context("Failed to read state database metadata during readiness probe")?;
        ensure!(metadata_rows >= 2, "State database metadata is incomplete");
        transaction
            .execute(
                "INSERT INTO store_meta(key, value) VALUES ('readiness-probe', X'00')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .context("Failed to write the state database readiness probe")?;
        transaction
            .rollback()
            .context("Failed to roll back the state database readiness probe")?;
        Ok(())
    }
}
