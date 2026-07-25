use super::rooted_fs::RootedFs;
use anyhow::Result;
use std::{future::Future, path::Path};
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
        source: &'a Path,
        destination: &'a Path,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
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
        file.sync_all().await?;
        Ok(())
    }

    async fn replace_and_sync_parents(&self, source: &Path, destination: &Path) -> Result<()> {
        self.rooted_fs.rename_replace(source, destination).await?;
        Ok(())
    }
}

pub(super) async fn sync_file_to_storage(file: &fs::File) -> Result<()> {
    file.sync_all().await?;
    Ok(())
}

pub(super) async fn commit_staged_file<S>(
    storage: &S,
    file: fs::File,
    source: &Path,
    destination: &Path,
) -> Result<()>
where
    S: StorageDurability,
{
    storage.sync_file(&file).await?;
    drop(file);
    storage.replace_and_sync_parents(source, destination).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Step {
        FileSync,
        ReplaceAndParentSync,
    }

    struct FaultStorage {
        fail_at: Option<Step>,
        calls: Arc<Mutex<Vec<Step>>>,
    }

    impl FaultStorage {
        fn new(fail_at: Option<Step>) -> Self {
            Self {
                fail_at,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn record(&self, step: Step) -> Result<()> {
            self.calls.lock().unwrap().push(step);
            if self.fail_at == Some(step) {
                anyhow::bail!("injected {step:?} failure");
            }
            Ok(())
        }
    }

    impl StorageDurability for FaultStorage {
        async fn sync_file(&self, _file: &fs::File) -> Result<()> {
            self.record(Step::FileSync)
        }

        async fn replace_and_sync_parents(
            &self,
            _source: &Path,
            _destination: &Path,
        ) -> Result<()> {
            self.record(Step::ReplaceAndParentSync)
        }
    }

    fn temporary_async_file() -> fs::File {
        fs::File::from_std(tempfile::tempfile().unwrap())
    }

    #[tokio::test]
    async fn commit_orders_file_sync_before_atomic_replace_and_parent_sync() {
        let storage = FaultStorage::new(None);
        commit_staged_file(
            &storage,
            temporary_async_file(),
            Path::new("/stage"),
            Path::new("/target"),
        )
        .await
        .unwrap();
        assert_eq!(
            *storage.calls.lock().unwrap(),
            [Step::FileSync, Step::ReplaceAndParentSync]
        );
    }

    #[tokio::test]
    async fn commit_stops_at_each_injected_durability_failure() {
        for (failure, expected) in [
            (Step::FileSync, vec![Step::FileSync]),
            (
                Step::ReplaceAndParentSync,
                vec![Step::FileSync, Step::ReplaceAndParentSync],
            ),
        ] {
            let storage = FaultStorage::new(Some(failure));
            let result = commit_staged_file(
                &storage,
                temporary_async_file(),
                Path::new("/stage"),
                Path::new("/target"),
            )
            .await;
            assert!(result.is_err(), "failure={failure:?}");
            assert_eq!(*storage.calls.lock().unwrap(), expected);
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
        let storage = DurableStorage::new(RootedFs::new(temp.path()).unwrap());

        commit_staged_file(&storage, file, &source, &target)
            .await
            .unwrap();

        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "new");
    }
}
