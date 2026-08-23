use super::{
    blocking_io::{BlockingIoGate, blocking_io_gate},
    rooted_fs::{ReplaceAndSyncOutcome, ReplacementTargetIdentity, RootedFs},
};
use anyhow::Result;
use rustix::{fd::OwnedFd, fs::fsync, io::dup};
use std::{future::Future, io, path::Path};
use tokio::fs;

/// Injectable durability boundary for the upload transaction.
///
/// HTTP handling does not decide how a file is synchronized or committed.
/// Tests can replace this boundary to fail either stage deterministically,
/// while production uses the rooted Linux filesystem implementation below.
pub(super) trait StorageDurability {
    fn sync_file<'a>(&'a self, file: &'a fs::File) -> impl Future<Output = Result<()>> + Send + 'a;

    fn replace_and_sync_parents<'a>(
        &'a self,
        file: &'a fs::File,
        source: &'a Path,
        destination: &'a Path,
        expected_destination: ReplacementTargetIdentity,
    ) -> impl Future<Output = ReplaceAndSyncOutcome> + Send + 'a;
}

#[derive(Debug)]
pub(super) enum CommitStagedFileOutcome {
    Published,
    Rejected(fs::File),
    NotPublished(anyhow::Error),
    PublishedDurabilityUnknown(anyhow::Error),
}

#[derive(Clone)]
pub(super) struct DurableStorage {
    rooted_fs: RootedFs,
}

impl DurableStorage {
    pub(super) fn new(rooted_fs: RootedFs) -> Self {
        Self { rooted_fs }
    }
}

impl StorageDurability for DurableStorage {
    async fn sync_file(&self, file: &fs::File) -> Result<()> {
        sync_file_with_gate(file, blocking_io_gate()).await
    }

    async fn replace_and_sync_parents(
        &self,
        file: &fs::File,
        source: &Path,
        destination: &Path,
        expected_destination: ReplacementTargetIdentity,
    ) -> ReplaceAndSyncOutcome {
        self.rooted_fs
            .rename_replace_if_unchanged(source, file, destination, expected_destination)
            .await
    }
}

pub(super) async fn sync_file_to_storage(file: &fs::File) -> Result<()> {
    sync_file_with_gate(file, blocking_io_gate()).await
}

async fn sync_file_with_gate(file: &fs::File, gate: &BlockingIoGate) -> Result<()> {
    let file = dup(file).map_err(io::Error::from)?;
    sync_owned_file_with_gate(file, gate).await
}

async fn sync_owned_file_with_gate(file: OwnedFd, gate: &BlockingIoGate) -> Result<()> {
    // The blocking worker owns a duplicate descriptor as well as its gate
    // permit. Dropping an HTTP waiter therefore cannot close the descriptor or
    // release admission while an already-started fsync remains in the kernel.
    gate.run_io(move || fsync(&file).map_err(io::Error::from))
        .await?;
    Ok(())
}

pub(super) async fn commit_staged_file<S>(
    storage: &S,
    file: fs::File,
    source: &Path,
    destination: &Path,
    expected_destination: ReplacementTargetIdentity,
) -> CommitStagedFileOutcome
where
    S: StorageDurability,
{
    if let Err(error) = storage.sync_file(&file).await {
        return CommitStagedFileOutcome::NotPublished(error);
    }
    let outcome = storage
        .replace_and_sync_parents(&file, source, destination, expected_destination)
        .await;
    match outcome {
        ReplaceAndSyncOutcome::Published => {
            drop(file);
            CommitStagedFileOutcome::Published
        }
        ReplaceAndSyncOutcome::Rejected => CommitStagedFileOutcome::Rejected(file),
        ReplaceAndSyncOutcome::NotPublished(error) => {
            drop(file);
            CommitStagedFileOutcome::NotPublished(error.into())
        }
        ReplaceAndSyncOutcome::PublishedDurabilityUnknown(error) => {
            drop(file);
            CommitStagedFileOutcome::PublishedDurabilityUnknown(error.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Step {
        FileSync,
        ReplaceAndParentSync,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ReplaceFailure {
        NotPublished,
        PublishedDurabilityUnknown,
    }

    struct FaultStorage {
        file_sync_failure: bool,
        replace_failure: Option<ReplaceFailure>,
        calls: Arc<Mutex<Vec<Step>>>,
    }

    impl FaultStorage {
        fn new(file_sync_failure: bool, replace_failure: Option<ReplaceFailure>) -> Self {
            Self {
                file_sync_failure,
                replace_failure,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn record(&self, step: Step) {
            self.calls.lock().unwrap().push(step);
        }
    }

    impl StorageDurability for FaultStorage {
        async fn sync_file(&self, _file: &fs::File) -> Result<()> {
            self.record(Step::FileSync);
            if self.file_sync_failure {
                anyhow::bail!("injected file sync failure");
            }
            Ok(())
        }

        async fn replace_and_sync_parents(
            &self,
            _file: &fs::File,
            _source: &Path,
            _destination: &Path,
            _expected_destination: ReplacementTargetIdentity,
        ) -> ReplaceAndSyncOutcome {
            self.record(Step::ReplaceAndParentSync);
            match self.replace_failure {
                None => ReplaceAndSyncOutcome::Published,
                Some(ReplaceFailure::NotPublished) => ReplaceAndSyncOutcome::NotPublished(
                    std::io::Error::other("injected pre-rename failure"),
                ),
                Some(ReplaceFailure::PublishedDurabilityUnknown) => {
                    ReplaceAndSyncOutcome::PublishedDurabilityUnknown(std::io::Error::other(
                        "injected post-rename sync failure",
                    ))
                }
            }
        }
    }

    fn temporary_async_file() -> fs::File {
        fs::File::from_std(tempfile::tempfile().unwrap())
    }

    #[tokio::test]
    async fn file_sync_waits_for_bounded_blocking_admission() {
        let gate = BlockingIoGate::with_capacity_for_test(1);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocker_gate = gate.clone();
        let blocker = tokio::spawn(async move {
            blocker_gate
                .run_io(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocking worker did not acquire admission")
        })
        .await
        .unwrap();

        let file = temporary_async_file();
        let mut syncing = Box::pin(sync_file_with_gate(&file, &gate));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut syncing)
                .await
                .is_err(),
            "fsync bypassed the saturated blocking I/O gate"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), syncing)
            .await
            .expect("fsync remained blocked after admission was released")
            .unwrap();
        blocker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn duplicated_descriptor_keeps_the_sync_target_open() {
        let gate = BlockingIoGate::with_capacity_for_test(1);
        let file = temporary_async_file();
        let duplicated = dup(&file).unwrap();
        drop(file);

        sync_owned_file_with_gate(duplicated, &gate).await.unwrap();
    }

    #[tokio::test]
    async fn commit_orders_file_sync_before_atomic_replace_and_parent_sync() {
        let storage = FaultStorage::new(false, None);
        let outcome = commit_staged_file(
            &storage,
            temporary_async_file(),
            Path::new("/stage"),
            Path::new("/target"),
            ReplacementTargetIdentity::Missing,
        )
        .await;
        assert!(matches!(outcome, CommitStagedFileOutcome::Published));
        assert_eq!(
            *storage.calls.lock().unwrap(),
            [Step::FileSync, Step::ReplaceAndParentSync]
        );
    }

    #[tokio::test]
    async fn commit_classifies_each_injected_durability_failure() {
        for (file_sync_failure, replace_failure, expected, expected_calls) in [
            (true, None, "not_published", vec![Step::FileSync]),
            (
                false,
                Some(ReplaceFailure::NotPublished),
                "not_published",
                vec![Step::FileSync, Step::ReplaceAndParentSync],
            ),
            (
                false,
                Some(ReplaceFailure::PublishedDurabilityUnknown),
                "published_unknown",
                vec![Step::FileSync, Step::ReplaceAndParentSync],
            ),
        ] {
            let storage = FaultStorage::new(file_sync_failure, replace_failure);
            let result = commit_staged_file(
                &storage,
                temporary_async_file(),
                Path::new("/stage"),
                Path::new("/target"),
                ReplacementTargetIdentity::Missing,
            )
            .await;
            match expected {
                "not_published" => {
                    assert!(matches!(result, CommitStagedFileOutcome::NotPublished(_)));
                }
                "published_unknown" => {
                    assert!(matches!(
                        result,
                        CommitStagedFileOutcome::PublishedDurabilityUnknown(_)
                    ));
                }
                _ => unreachable!(),
            }
            assert_eq!(*storage.calls.lock().unwrap(), expected_calls);
        }
    }

    #[tokio::test]
    async fn production_storage_replaces_the_target_inside_the_opened_root() {
        let temp = assert_fs::TempDir::new().unwrap();
        let source = temp.path().join("stage");
        let target = temp.path().join("target");
        std::fs::write(&source, "new").unwrap();
        std::fs::write(&target, "old").unwrap();
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&source)
            .await
            .unwrap();
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        let expected_destination = rooted_fs
            .replacement_metadata(&target)
            .await
            .unwrap()
            .identity;
        let storage = DurableStorage::new(rooted_fs);

        assert!(matches!(
            commit_staged_file(&storage, file, &source, &target, expected_destination).await,
            CommitStagedFileOutcome::Published
        ));

        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "new");
    }
}
