use anyhow::{Context, Result, anyhow};
use rustix::{
    fd::OwnedFd,
    fs::{
        AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, ResolveFlags, fsync, mkdirat, openat,
        openat2, renameat, renameat_with, statat, unlinkat,
    },
    io::{Errno, dup},
};
use std::{
    ffi::OsString,
    fs::File,
    os::fd::AsRawFd,
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::task;
use uuid::Uuid;

#[derive(Clone)]
pub(super) struct RootedFs {
    inner: Arc<RootedFsInner>,
}

struct RootedFsInner {
    root: File,
    root_path: PathBuf,
    resolve: ResolveFlags,
    ancestor_creation: Mutex<()>,
}

struct OpenedParent {
    fd: OwnedFd,
    name: OsString,
    created_ancestors: bool,
}

pub(super) struct TrashEntry {
    parent: File,
    name: OsString,
    is_dir: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct RootedEntryKey {
    parent: FileIdentity,
    name: OsString,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SemanticPathComponent {
    Directory(FileIdentity),
    Name(OsString),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResolvedPathKey {
    pub(super) components: Vec<SemanticPathComponent>,
    pub(super) target_directory: Option<FileIdentity>,
}

#[derive(Debug)]
pub(super) struct RootedDirEntry {
    pub(super) path: PathBuf,
    pub(super) file_name: OsString,
    pub(super) metadata: std::fs::Metadata,
    pub(super) is_symlink: bool,
}

impl RootedFs {
    pub(super) fn new(root_path: &Path) -> Result<Self> {
        let root = File::open(root_path)
            .with_context(|| format!("Failed to open shared root `{}`", root_path.display()))?;
        if !root.metadata()?.is_dir() {
            return Err(anyhow!(
                "The rooted filesystem anchor is not a directory: `{}`",
                root_path.display()
            ));
        }
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
        openat2(
            &root,
            ".",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            resolve,
        )
        .map_err(std::io::Error::from)
        .context("Linux openat2 is required for safe rooted file operations")?;

        Ok(Self {
            inner: Arc::new(RootedFsInner {
                root,
                root_path: root_path.to_path_buf(),
                resolve,
                ancestor_creation: Mutex::new(()),
            }),
        })
    }

    pub(super) async fn metadata_nofollow(
        &self,
        path: &Path,
    ) -> std::io::Result<std::fs::Metadata> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let relative = this.relative_path_or_dot(&path)?;
            let fd = openat2(
                &this.inner.root,
                relative,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                this.inner.resolve,
            )
            .map_err(std::io::Error::from)?;
            File::from(fd).metadata()
        })
        .await
    }

    pub(super) async fn metadata(&self, path: &Path) -> std::io::Result<std::fs::Metadata> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || this.metadata_blocking(&path)).await
    }

    pub(super) fn metadata_blocking(&self, path: &Path) -> std::io::Result<std::fs::Metadata> {
        let relative = self.relative_path_or_dot(path)?;
        let fd = openat2(
            &self.inner.root,
            relative,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            self.inner.resolve,
        )
        .map_err(std::io::Error::from)?;
        File::from(fd).metadata()
    }

    pub(super) fn visit_dir_blocking<F>(&self, path: &Path, mut visitor: F) -> std::io::Result<()>
    where
        F: FnMut(RootedDirEntry) -> std::io::Result<bool>,
    {
        self.visit_dir_blocking_bounded(path, |_| true, &mut visitor)
    }

    /// Visit one directory without first collecting its entries.
    ///
    /// `budget` runs immediately after `readdir` and before resolving metadata
    /// for that entry. Returning `false` stops enumeration, so callers can
    /// enforce entry and time budgets even for one exceptionally large
    /// directory without performing an unbounded metadata pass first.
    pub(super) fn visit_dir_blocking_bounded<B, F>(
        &self,
        path: &Path,
        mut budget: B,
        mut visitor: F,
    ) -> std::io::Result<()>
    where
        B: FnMut(&Path) -> bool,
        F: FnMut(RootedDirEntry) -> std::io::Result<bool>,
    {
        let relative = self.relative_path_or_dot(path)?;
        let directory = self.open_directory_from_root(relative)?;
        let mut directory = Dir::new(directory).map_err(std::io::Error::from)?;

        while let Some(entry) = directory.read() {
            let entry = entry.map_err(std::io::Error::from)?;
            let name = entry.file_name().to_bytes();
            if matches!(name, b"." | b"..") {
                continue;
            }
            let file_name = OsString::from_vec(name.to_vec());
            let child_relative = if relative == Path::new(".") {
                PathBuf::from(&file_name)
            } else {
                relative.join(&file_name)
            };
            let child_path = self.inner.root_path.join(&child_relative);
            if !budget(&child_path) {
                break;
            }
            let directory_fd = directory.fd().map_err(std::io::Error::from)?;
            let nofollow = statat(directory_fd, &file_name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let is_symlink = FileType::from_raw_mode(nofollow.st_mode) == FileType::Symlink;
            let target = match openat2(
                &self.inner.root,
                &child_relative,
                OFlags::PATH | OFlags::CLOEXEC,
                Mode::empty(),
                self.inner.resolve,
            ) {
                Ok(target) => target,
                // RESOLVE_BENEATH reports EXDEV for absolute links and links
                // which leave the shared root. Such entries are deliberately
                // invisible to the browser.
                Err(Errno::XDEV) => continue,
                // A dangling relative link, or a relative link cycle, is still
                // an entry inside the managed root. Open the final component
                // itself so the browser can show and manage it without ever
                // following the unresolved target. Do not apply this fallback
                // to XDEV above: absolute and root-escaping links stay hidden.
                Err(Errno::NOENT | Errno::LOOP) if is_symlink => openat2(
                    &self.inner.root,
                    &child_relative,
                    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                    self.inner.resolve,
                )
                .map_err(std::io::Error::from)?,
                Err(error) => return Err(std::io::Error::from(error)),
            };
            let metadata = File::from(target).metadata()?;
            let entry = RootedDirEntry {
                path: child_path,
                file_name,
                metadata,
                is_symlink,
            };
            if !visitor(entry)? {
                break;
            }
        }
        Ok(())
    }

    pub(super) async fn resolved_path_key(&self, path: &Path) -> std::io::Result<ResolvedPathKey> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || this.resolved_path_key_blocking(&path)).await
    }

    fn resolved_path_key_blocking(&self, path: &Path) -> std::io::Result<ResolvedPathKey> {
        let relative = self.relative_path(path)?;
        let components = relative
            .components()
            .map(|component| match component {
                Component::Normal(value) => Ok(value.to_os_string()),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path contains a non-normal component",
                )),
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let root_identity = file_identity(&self.inner.root.metadata()?);
        let mut semantic = vec![SemanticPathComponent::Directory(root_identity)];
        let mut prefix = PathBuf::new();
        let mut resolving_directories = true;

        for component in components.iter().take(components.len().saturating_sub(1)) {
            prefix.push(component);
            if resolving_directories {
                match self.open_directory_from_root(&prefix) {
                    Ok(directory) => {
                        semantic.push(SemanticPathComponent::Directory(file_identity(
                            &File::from(directory).metadata()?,
                        )));
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::NotADirectory
                                | std::io::ErrorKind::PermissionDenied
                        ) =>
                    {
                        resolving_directories = false;
                        semantic.push(SemanticPathComponent::Name(component.clone()));
                    }
                    Err(error) => return Err(error),
                }
            } else {
                semantic.push(SemanticPathComponent::Name(component.clone()));
            }
        }

        let final_name = components
            .last()
            .ok_or_else(|| std::io::Error::other("the shared root has no entry key"))?
            .clone();
        semantic.push(SemanticPathComponent::Name(final_name.clone()));

        let target_directory = if resolving_directories {
            let parent = self.open_parent_blocking(path, false)?;
            match statat(&parent.fd, &parent.name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => {
                    let directory = openat(
                        &parent.fd,
                        &parent.name,
                        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(std::io::Error::from)?;
                    Some(file_identity(&File::from(directory).metadata()?))
                }
                Ok(_) | Err(Errno::NOENT) => None,
                Err(error) => return Err(std::io::Error::from(error)),
            }
        } else {
            None
        };

        Ok(ResolvedPathKey {
            components: semantic,
            target_directory,
        })
    }

    #[cfg(test)]
    pub(super) async fn is_resolved_beneath(&self, path: &Path) -> bool {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let relative = this.relative_path(&path)?;
            openat2(
                &this.inner.root,
                relative,
                OFlags::PATH | OFlags::CLOEXEC,
                Mode::empty(),
                this.inner.resolve,
            )
            .map_err(std::io::Error::from)?;
            Ok(())
        })
        .await
        .is_ok()
    }

    pub(super) async fn open_read(&self, path: &Path) -> std::io::Result<tokio::fs::File> {
        self.open_existing(path, OFlags::RDONLY | OFlags::CLOEXEC)
            .await
    }

    pub(super) async fn open_read_nofollow(&self, path: &Path) -> std::io::Result<tokio::fs::File> {
        self.open_existing(path, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC)
            .await
    }

    pub(super) async fn open_write(&self, path: &Path) -> std::io::Result<tokio::fs::File> {
        self.open_existing(path, OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC)
            .await
    }

    async fn open_existing(&self, path: &Path, flags: OFlags) -> std::io::Result<tokio::fs::File> {
        let this = self.clone();
        let path = path.to_path_buf();
        let file = run_blocking(move || {
            let relative = this.relative_path(&path)?;
            let fd = openat2(
                &this.inner.root,
                relative,
                flags,
                Mode::empty(),
                this.inner.resolve,
            )
            .map_err(std::io::Error::from)?;
            Ok(File::from(fd))
        })
        .await?;
        Ok(tokio::fs::File::from_std(file))
    }

    pub(super) async fn create_new(&self, path: &Path) -> std::io::Result<(tokio::fs::File, bool)> {
        let this = self.clone();
        let path = path.to_path_buf();
        let (file, created_ancestors) = run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let parent = this.open_parent_blocking(&path, true)?;
            let fd = openat(
                &parent.fd,
                &parent.name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o666),
            )
            .map_err(std::io::Error::from)?;
            Ok((File::from(fd), parent.created_ancestors))
        })
        .await?;
        Ok((tokio::fs::File::from_std(file), created_ancestors))
    }

    pub(super) async fn create_directory(&self, path: &Path) -> std::io::Result<bool> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let parent = this.open_parent_blocking(&path, true)?;
            mkdirat(&parent.fd, &parent.name, Mode::from_raw_mode(0o777))
                .map_err(std::io::Error::from)?;
            fsync(&parent.fd).map_err(std::io::Error::from)?;
            Ok(parent.created_ancestors)
        })
        .await
    }

    pub(super) async fn ensure_parent(&self, path: &Path) -> std::io::Result<bool> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(this.open_parent_blocking(&path, true)?.created_ancestors)
        })
        .await
    }

    pub(super) async fn entry_key(&self, path: &Path) -> std::io::Result<RootedEntryKey> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || this.entry_key_blocking(&path)).await
    }

    pub(super) fn entry_key_blocking(&self, path: &Path) -> std::io::Result<RootedEntryKey> {
        let parent = self.open_parent_blocking(path, false)?;
        let parent_file = File::from(dup(&parent.fd).map_err(std::io::Error::from)?);
        Ok(RootedEntryKey {
            parent: file_identity(&parent_file.metadata()?),
            name: parent.name,
        })
    }

    pub(super) fn remove_file_if_exists_blocking(&self, path: &Path) -> std::io::Result<bool> {
        let parent = match self.open_parent_blocking(path, false) {
            Ok(parent) => parent,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        match unlinkat(&parent.fd, &parent.name, AtFlags::empty()) {
            Ok(()) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(err) => Err(std::io::Error::from(err)),
        }
    }

    pub(super) fn remove_entry_blocking(&self, path: &Path, is_dir: bool) -> std::io::Result<bool> {
        let parent = match self.open_parent_blocking(path, false) {
            Ok(parent) => parent,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        if is_dir {
            let proc_path = PathBuf::from(format!(
                "/proc/self/fd/{}/{}",
                parent.fd.as_raw_fd(),
                parent.name.to_string_lossy()
            ));
            match std::fs::remove_dir_all(proc_path) {
                Ok(()) => Ok(true),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(err) => Err(err),
            }
        } else {
            match unlinkat(&parent.fd, &parent.name, AtFlags::empty()) {
                Ok(()) => Ok(true),
                Err(Errno::NOENT) => Ok(false),
                Err(err) => Err(std::io::Error::from(err)),
            }
        }
    }

    pub(super) async fn remove_file_if_exists_durable(&self, path: &Path) -> std::io::Result<bool> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let parent = match this.open_parent_blocking(&path, false) {
                Ok(parent) => parent,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(err) => return Err(err),
            };
            match unlinkat(&parent.fd, &parent.name, AtFlags::empty()) {
                Ok(()) => {
                    fsync(&parent.fd).map_err(std::io::Error::from)?;
                    Ok(true)
                }
                Err(Errno::NOENT) => Ok(false),
                Err(err) => Err(std::io::Error::from(err)),
            }
        })
        .await
    }

    pub(super) async fn rename_replace(
        &self,
        source: &Path,
        destination: &Path,
    ) -> std::io::Result<()> {
        let this = self.clone();
        let source = source.to_path_buf();
        let destination = destination.to_path_buf();
        run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let source = this.open_parent_blocking(&source, false)?;
            let destination = this.open_parent_blocking(&destination, true)?;
            renameat(&source.fd, &source.name, &destination.fd, &destination.name)
                .map_err(std::io::Error::from)?;
            fsync(&source.fd).map_err(std::io::Error::from)?;
            fsync(&destination.fd).map_err(std::io::Error::from)
        })
        .await
    }

    pub(super) async fn rename_no_replace(
        &self,
        source: &Path,
        destination: &Path,
    ) -> std::io::Result<bool> {
        let this = self.clone();
        let source = source.to_path_buf();
        let destination = destination.to_path_buf();
        run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let source = this.open_parent_blocking(&source, false)?;
            let destination = this.open_parent_blocking(&destination, true)?;
            match renameat_with(
                &source.fd,
                &source.name,
                &destination.fd,
                &destination.name,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => {
                    fsync(&source.fd).map_err(std::io::Error::from)?;
                    fsync(&destination.fd).map_err(std::io::Error::from)?;
                    Ok(true)
                }
                Err(Errno::EXIST) => Ok(false),
                Err(err) => Err(std::io::Error::from(err)),
            }
        })
        .await
    }

    pub(super) async fn sync_parent(&self, path: &Path) -> std::io::Result<()> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let parent = this.open_parent_blocking(&path, false)?;
            fsync(&parent.fd).map_err(std::io::Error::from)
        })
        .await
    }

    pub(super) async fn move_to_trash(&self, path: &Path) -> std::io::Result<TrashEntry> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let parent = this.open_parent_blocking(&path, false)?;
            let metadata = statat(&parent.fd, &parent.name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let is_dir = FileType::from_raw_mode(metadata.st_mode) == FileType::Directory;
            for _ in 0..16 {
                let name = OsString::from(format!(".dufs-upload-delete-{}.trash", Uuid::new_v4()));
                match renameat_with(
                    &parent.fd,
                    &parent.name,
                    &parent.fd,
                    &name,
                    RenameFlags::NOREPLACE,
                ) {
                    Ok(()) => {
                        fsync(&parent.fd).map_err(std::io::Error::from)?;
                        return Ok(TrashEntry {
                            parent: File::from(parent.fd),
                            name,
                            is_dir,
                        });
                    }
                    Err(Errno::EXIST) => continue,
                    Err(err) => return Err(std::io::Error::from(err)),
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "failed to allocate a unique internal trash name",
            ))
        })
        .await
    }

    fn open_parent_blocking(
        &self,
        path: &Path,
        create_ancestors: bool,
    ) -> std::io::Result<OpenedParent> {
        let relative = self.relative_path(path)?;
        let name = relative
            .file_name()
            .ok_or_else(|| std::io::Error::other("the shared root has no parent entry"))?
            .to_os_string();
        let relative_parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let mut current = dup(&self.inner.root).map_err(std::io::Error::from)?;
        let mut prefix = PathBuf::new();
        let mut created = false;

        for component in relative_parent.components() {
            let Component::Normal(component) = component else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path contains a non-normal component",
                ));
            };
            prefix.push(component);
            match self.open_directory_from_root(&prefix) {
                Ok(next) => current = next,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound && create_ancestors => {
                    match mkdirat(&current, component, Mode::from_raw_mode(0o777)) {
                        Ok(()) => {
                            fsync(&current).map_err(std::io::Error::from)?;
                            created = true;
                        }
                        // Another sibling mutation may have created this
                        // ancestor after openat2 reported ENOENT. It is not
                        // enough to rely on that request's later fsync: this
                        // request can finish first, so it must independently
                        // make the shared ancestor entry durable.
                        Err(Errno::EXIST) => {
                            fsync(&current).map_err(std::io::Error::from)?;
                        }
                        Err(err) => return Err(std::io::Error::from(err)),
                    }
                    current = self.open_directory_from_root(&prefix)?;
                }
                Err(err) => return Err(err),
            }
        }

        Ok(OpenedParent {
            fd: current,
            name,
            created_ancestors: created,
        })
    }

    fn open_directory_from_root(&self, relative: &Path) -> std::io::Result<OwnedFd> {
        let relative = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        openat2(
            &self.inner.root,
            relative,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            self.inner.resolve,
        )
        .map_err(std::io::Error::from)
    }

    fn relative_path<'a>(&self, path: &'a Path) -> std::io::Result<&'a Path> {
        let relative = path.strip_prefix(&self.inner.root_path).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "path `{}` is outside rooted filesystem `{}`",
                    path.display(),
                    self.inner.root_path.display()
                ),
            )
        })?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path must name a non-root entry",
            ));
        }
        Ok(relative)
    }

    fn relative_path_or_dot<'a>(&self, path: &'a Path) -> std::io::Result<&'a Path> {
        let relative = path.strip_prefix(&self.inner.root_path).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path is outside the rooted filesystem",
            )
        })?;
        if relative.as_os_str().is_empty() {
            return Ok(Path::new("."));
        }
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains a non-normal component",
            ));
        }
        Ok(relative)
    }
}

fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

impl TrashEntry {
    pub(super) async fn purge(self) -> std::io::Result<()> {
        run_blocking(move || {
            if self.is_dir {
                let path = PathBuf::from(format!(
                    "/proc/self/fd/{}/{}",
                    self.parent.as_raw_fd(),
                    self.name.to_string_lossy()
                ));
                match std::fs::remove_dir_all(path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            } else {
                match unlinkat(&self.parent, &self.name, AtFlags::empty()) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(err) => return Err(std::io::Error::from(err)),
                }
            }
            fsync(&self.parent).map_err(std::io::Error::from)
        })
        .await
    }
}

async fn run_blocking<T, F>(task: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    task::spawn_blocking(task)
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn creates_and_opens_files_beneath_root() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted = RootedFs::new(temp.path()).unwrap();
        let path = temp.path().join("a/b/file.txt");
        let (mut file, created) = rooted.create_new(&path).await.unwrap();
        assert!(created);
        file.write_all(b"content").await.unwrap();
        file.sync_all().await.unwrap();
        drop(file);

        let mut file = rooted.open_read(&path).await.unwrap().into_std().await;
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "content");
    }

    #[test]
    fn bounded_directory_visit_stops_before_resolving_the_over_budget_entry() {
        const TOTAL_ENTRIES: usize = 128;
        const ENTRY_BUDGET: usize = 4;

        let temp = assert_fs::TempDir::new().unwrap();
        for index in 0..TOTAL_ENTRIES {
            std::fs::write(temp.path().join(format!("entry-{index:03}")), []).unwrap();
        }
        let rooted = RootedFs::new(temp.path()).unwrap();
        let mut budget_checks = 0usize;
        let mut resolved_entries = 0usize;

        rooted
            .visit_dir_blocking_bounded(
                temp.path(),
                |_| {
                    budget_checks += 1;
                    budget_checks <= ENTRY_BUDGET
                },
                |_| {
                    resolved_entries += 1;
                    Ok(true)
                },
            )
            .unwrap();

        assert_eq!(budget_checks, ENTRY_BUDGET + 1);
        assert_eq!(resolved_entries, ENTRY_BUDGET);
        assert!(resolved_entries < TOTAL_ENTRIES);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ancestor_creation_lock_covers_directory_publication() {
        use std::time::Duration;

        let temp = assert_fs::TempDir::new().unwrap();
        let rooted = RootedFs::new(temp.path()).unwrap();
        let guard = rooted
            .inner
            .ancestor_creation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = temp.path().join("shared-parent");
        let creator = {
            let rooted = rooted.clone();
            let directory = directory.clone();
            tokio::spawn(async move { rooted.create_directory(&directory).await })
        };

        std::thread::sleep(Duration::from_millis(20));
        assert!(!creator.is_finished());
        assert!(!directory.exists());

        drop(guard);
        creator.await.unwrap().unwrap();
        assert!(directory.is_dir());
    }

    #[tokio::test]
    async fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = assert_fs::TempDir::new().unwrap();
        let outside = assert_fs::TempDir::new().unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let rooted = RootedFs::new(root.path()).unwrap();
        assert!(
            !rooted
                .is_resolved_beneath(&root.path().join("escape"))
                .await
        );
        let err = rooted
            .create_new(&root.path().join("escape/file.txt"))
            .await
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(18));
        assert!(!outside.path().join("file.txt").exists());
    }

    #[tokio::test]
    async fn allows_symlink_parent_that_stays_beneath_root() {
        use std::os::unix::fs::symlink;

        let root = assert_fs::TempDir::new().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink("target", root.path().join("alias")).unwrap();
        let rooted = RootedFs::new(root.path()).unwrap();
        let path = root.path().join("alias/file.txt");
        assert!(rooted.is_resolved_beneath(&root.path().join("alias")).await);

        let (mut file, created) = rooted.create_new(&path).await.unwrap();
        assert!(!created);
        file.write_all(b"content").await.unwrap();
        file.sync_all().await.unwrap();
        drop(file);

        assert_eq!(
            std::fs::read_to_string(target.join("file.txt")).unwrap(),
            "content"
        );
    }

    #[tokio::test]
    async fn no_replace_preserves_competing_destination() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted = RootedFs::new(temp.path()).unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::write(&source, "source").unwrap();
        std::fs::write(&destination, "destination").unwrap();

        assert!(
            !rooted
                .rename_no_replace(&source, &destination)
                .await
                .unwrap()
        );
        assert_eq!(std::fs::read_to_string(source).unwrap(), "source");
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "destination");
    }

    #[tokio::test]
    async fn directory_discovery_remains_anchored_after_root_path_replacement() {
        let parent = assert_fs::TempDir::new().unwrap();
        let root = parent.path().join("root");
        let original = parent.path().join("original");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("original.txt"), "original").unwrap();
        let rooted = RootedFs::new(&root).unwrap();

        std::fs::rename(&root, &original).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("replacement.txt"), "replacement").unwrap();

        let mut entries = Vec::new();
        rooted
            .visit_dir_blocking(&root, |entry| {
                entries.push(entry);
                Ok(true)
            })
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name, "original.txt");
        assert_eq!(
            std::fs::read_to_string(original.join("original.txt")).unwrap(),
            "original"
        );
    }

    #[tokio::test]
    async fn durable_delete_hides_directory_before_background_purge() {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted = RootedFs::new(temp.path()).unwrap();
        let directory = temp.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("file.txt"), "content").unwrap();

        let trash = rooted.move_to_trash(&directory).await.unwrap();
        assert!(!directory.exists());
        assert!(std::fs::read_dir(temp.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".dufs-upload-delete-")
        }));

        trash.purge().await.unwrap();
        assert!(!std::fs::read_dir(temp.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".dufs-upload-delete-")
        }));
    }
}
