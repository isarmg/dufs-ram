use super::{
    blocking_io::blocking_io_gate,
    internal_names::{
        InternalEntryName, classify_internal_name, delete_trash_name, quarantine_name,
        upload_readiness_probe_name,
    },
};
use anyhow::{Context, Result, anyhow};
use rustix::{
    fd::OwnedFd,
    fs::{
        AtFlags, Dir, FileType, FlockOperation, Mode, OFlags, RenameFlags, ResolveFlags,
        XattrFlags, fchmod, fchown, fgetxattr, flistxattr, flock, fremovexattr, fsetxattr, fstat,
        fsync, mkdirat, openat, openat2, renameat, renameat_with, statat, unlinkat,
    },
    io::{Errno, dup},
    process::{Gid, Uid},
};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString, OsString},
    fs::File,
    io::Write,
    os::fd::AsFd,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

mod purge;

#[allow(unused_imports)]
pub(super) use purge::TrashPurgeError;
use purge::remove_directory_at;
pub(super) use purge::{TrashEntry, TrashPurgeProgress};

#[cfg(test)]
use std::time::{Duration, Instant};
#[cfg(test)]
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct RootedFs {
    inner: Arc<RootedFsInner>,
}

struct RootedFsInner {
    root: File,
    root_identity: FileIdentity,
    root_path: PathBuf,
    resolve: ResolveFlags,
    ancestor_creation: Mutex<()>,
    #[cfg(test)]
    before_missing_rename: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct OpenedParent {
    fd: OwnedFd,
    name: OsString,
    created_ancestors: Vec<CreatedAncestor>,
}

#[derive(Debug)]
struct CreatedAncestor {
    path: PathBuf,
    identity: FileIdentity,
}

#[derive(Debug)]
pub(super) struct CreatedAncestors {
    entries: Vec<CreatedAncestor>,
}

#[derive(Debug)]
pub(super) struct PreservedFileMetadata {
    uid: u32,
    gid: u32,
    mode: u32,
    xattrs: Vec<(CString, Vec<u8>)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ExistingReplacementTarget {
    device: u64,
    inode: u64,
    file_type: FileType,
    links: u64,
    size: i64,
    uid: u32,
    gid: u32,
    mode: u32,
    modified_seconds: i64,
    modified_nanoseconds: u64,
    changed_seconds: i64,
    changed_nanoseconds: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReplacementTargetIdentity {
    Missing,
    Existing(ExistingReplacementTarget),
}

impl ReplacementTargetIdentity {
    pub(super) const fn exists(self) -> bool {
        matches!(self, Self::Existing(_))
    }

    pub(super) fn delete_identity(self) -> Option<DeleteIdentity> {
        let Self::Existing(target) = self else {
            return None;
        };
        Some(DeleteIdentity {
            device: target.device,
            inode: target.inode,
            is_directory: target.file_type == FileType::Directory,
        })
    }

    /// Whether the inexpensive target inspection found an object type whose
    /// metadata can safely participate in the upload replacement protocol.
    /// The later metadata capture remains authoritative (notably for xattrs),
    /// so this is a preflight hint rather than a publication guarantee.
    pub(super) const fn is_replaceable(self) -> bool {
        match self {
            Self::Missing => true,
            Self::Existing(target) => match target.file_type {
                FileType::Symlink => true,
                FileType::RegularFile => target.links == 1 && target.mode & 0o6000 == 0,
                _ => false,
            },
        }
    }

    /// Add the complete equality identity to an opaque revision digest.
    /// Keeping this beside the type makes it difficult to add a new CAS field
    /// without also invalidating preflight revisions derived from old data.
    pub(super) fn update_revision_digest(self, digest: &mut Sha256) {
        match self {
            Self::Missing => digest.update([0]),
            Self::Existing(target) => {
                digest.update([1]);
                digest.update(target.device.to_be_bytes());
                digest.update(target.inode.to_be_bytes());
                digest.update(file_type_revision_tag(target.file_type));
                digest.update(target.links.to_be_bytes());
                digest.update(target.size.to_be_bytes());
                digest.update(target.uid.to_be_bytes());
                digest.update(target.gid.to_be_bytes());
                digest.update(target.mode.to_be_bytes());
                digest.update(target.modified_seconds.to_be_bytes());
                digest.update(target.modified_nanoseconds.to_be_bytes());
                digest.update(target.changed_seconds.to_be_bytes());
                digest.update(target.changed_nanoseconds.to_be_bytes());
            }
        }
    }

    pub(super) fn purge_revision(self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"dufs-purge-trash-revision-v1\0");
        self.update_revision_digest(&mut digest);
        digest.finalize().into()
    }
}

#[derive(Debug)]
pub(super) enum ReplaceAndSyncOutcome {
    Published,
    Rejected,
    NotPublished(std::io::Error),
    PublishedDurabilityUnknown(std::io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CheckedRelocationOutcome {
    Relocated,
    SourceChanged,
    DestinationChanged,
    DestinationExists,
    SameFile,
}

enum CheckedMissingRename {
    Renamed,
    DestinationExists,
    PublishedIdentityUnknown(std::io::Error),
}

enum PreparedCheckedReplace {
    Renamed(OpenedParent, OpenedParent, bool),
    Rejected,
    PublishedIdentityUnknown(std::io::Error),
}

#[derive(Debug)]
pub(super) struct ReplacementTarget {
    pub(super) identity: ReplacementTargetIdentity,
    pub(super) metadata: Option<PreservedFileMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DeleteIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) is_directory: bool,
}

pub(super) enum CheckedTrashMove {
    Moved(TrashEntry),
    TargetChanged,
    NotMoved(std::io::Error),
    DurabilityUnknown(std::io::Error),
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

impl RootedEntryKey {
    /// Return an in-memory-only key used to coordinate maintenance cleanup
    /// with upload admission. Linux path components cannot contain NUL, so
    /// this marker cannot collide with a real rooted entry key.
    pub(super) fn maintenance_marker(&self) -> Self {
        let mut name = self.name.clone().into_vec();
        name.extend_from_slice(b"\0dufs-maintenance");
        Self {
            parent: self.parent,
            name: OsString::from_vec(name),
        }
    }
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
    pub(super) revision_identity: ReplacementTargetIdentity,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DirectoryCursor(i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DirectoryVisitProgress {
    Complete,
    Paused(DirectoryCursor),
}

impl RootedFs {
    pub(super) fn new(root_path: &Path) -> Result<Self> {
        let root = File::open(root_path)
            .with_context(|| format!("Failed to open shared root `{}`", root_path.display()))?;
        let root_metadata = root.metadata()?;
        if !root_metadata.is_dir() {
            return Err(anyhow!(
                "The rooted filesystem anchor is not a directory: `{}`",
                root_path.display()
            ));
        }
        flock(&root, FlockOperation::NonBlockingLockExclusive)
            .map_err(std::io::Error::from)
            .with_context(|| {
                format!(
                    "Failed to acquire the single-instance lock for shared root `{}`",
                    root_path.display()
                )
            })?;
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
                root_identity: file_identity(&root_metadata),
                root,
                root_path: root_path.to_path_buf(),
                resolve,
                ancestor_creation: Mutex::new(()),
                #[cfg(test)]
                before_missing_rename: Mutex::new(None),
            }),
        })
    }

    #[cfg(test)]
    pub(super) fn inject_before_missing_rename_once(&self, hook: impl FnOnce() + Send + 'static) {
        let previous = self
            .inner
            .before_missing_rename
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(Box::new(hook));
        assert!(
            previous.is_none(),
            "a missing-rename hook is already installed"
        );
    }

    fn run_before_missing_rename_hook(&self) {
        #[cfg(test)]
        let hook = self
            .inner
            .before_missing_rename
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        #[cfg(test)]
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(super) fn root_handle(&self) -> &File {
        &self.inner.root
    }

    pub(super) fn root_identity(&self) -> (u64, u64) {
        (
            self.inner.root_identity.device,
            self.inner.root_identity.inode,
        )
    }

    /// Prove that the anchored shared root can create, write, sync, unlink,
    /// and durably record removal of a new entry. The generated name follows
    /// the upload-internal grammar, so even a process crash between create and
    /// unlink cannot expose a readiness artifact through listings.
    pub(super) async fn probe_writable(&self) -> std::io::Result<()> {
        let this = self.clone();
        run_blocking(move || {
            let name = upload_readiness_probe_name(Uuid::new_v4());
            let fd = openat(
                &this.inner.root,
                name.as_str(),
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(std::io::Error::from)?;
            let mut file = File::from(fd);
            let write_result = (|| {
                file.write_all(b"dufs-readiness-v1")?;
                file.sync_data()
            })();
            let remove_result = unlinkat(&this.inner.root, name.as_str(), AtFlags::empty())
                .map_err(std::io::Error::from);

            write_result?;
            remove_result?;
            fsync(&this.inner.root).map_err(std::io::Error::from)
        })
        .await
    }

    /// Encodes a validated, non-root path without depending on the spelling
    /// of the mounted shared-root path. Control-plane records can therefore be
    /// reopened after restart even when the same root inode is reached through
    /// a different mount alias.
    #[cfg(test)]
    pub(super) fn encode_relative_path(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        Ok(self.relative_path(path)?.as_os_str().as_bytes().to_vec())
    }

    pub(super) fn state_relative_path(&self, path: &Path) -> std::io::Result<PathBuf> {
        Ok(self.relative_path(path)?.to_path_buf())
    }

    /// Decodes an untrusted control-plane path and reattaches it to this
    /// instance's rooted namespace. Only normal relative components are
    /// accepted; containment is still enforced again by the fd-relative open.
    pub(super) fn decode_relative_path(&self, encoded: &[u8]) -> std::io::Result<PathBuf> {
        if encoded.is_empty() || encoded.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stored rooted path is empty or contains NUL",
            ));
        }
        let relative = PathBuf::from(OsString::from_vec(encoded.to_vec()));
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stored rooted path contains a non-normal component",
            ));
        }
        Ok(self.inner.root_path.join(relative))
    }

    pub(super) fn resolve_state_path(&self, relative: &Path) -> std::io::Result<PathBuf> {
        self.decode_relative_path(relative.as_os_str().as_bytes())
    }

    pub(super) fn conservative_path_key(&self) -> ResolvedPathKey {
        ResolvedPathKey {
            components: vec![SemanticPathComponent::Directory(self.inner.root_identity)],
            target_directory: Some(self.inner.root_identity),
        }
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
        self.visit_dir_blocking_chunk(path, DirectoryCursor::default(), &mut budget, &mut visitor)
            .map(|_| ())
    }

    /// Visit a resumable chunk of one directory.
    ///
    /// The cursor is the opaque `readdir` offset immediately after the last
    /// successfully visited entry. It avoids restarting an exceptionally wide
    /// directory on every maintenance slice. Returning `false` from `visitor`
    /// pauses after the current entry; failing the budget pauses before it.
    pub(super) fn visit_dir_blocking_chunk<B, F>(
        &self,
        path: &Path,
        cursor: DirectoryCursor,
        mut budget: B,
        mut visitor: F,
    ) -> std::io::Result<DirectoryVisitProgress>
    where
        B: FnMut(&Path) -> bool,
        F: FnMut(RootedDirEntry) -> std::io::Result<bool>,
    {
        let relative = self.relative_path_or_dot(path)?;
        let directory = self.open_directory_from_root(relative)?;
        let mut directory = Dir::new(directory).map_err(std::io::Error::from)?;
        if cursor.0 != 0 {
            directory.seek(cursor.0).map_err(std::io::Error::from)?;
        }
        let mut resume_cursor = cursor;

        while let Some(entry) = directory.read() {
            let entry = entry.map_err(std::io::Error::from)?;
            let name = entry.file_name().to_bytes();
            let next_cursor = DirectoryCursor(entry.offset());
            if matches!(name, b"." | b"..") {
                resume_cursor = next_cursor;
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
                return Ok(DirectoryVisitProgress::Paused(resume_cursor));
            }
            let directory_fd = directory.fd().map_err(std::io::Error::from)?;
            let nofollow = statat(directory_fd, &file_name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let is_symlink = FileType::from_raw_mode(nofollow.st_mode) == FileType::Symlink;
            let revision_identity =
                ReplacementTargetIdentity::Existing(replacement_target_identity(&nofollow));
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
                Err(Errno::XDEV) => {
                    resume_cursor = next_cursor;
                    continue;
                }
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
                revision_identity,
            };
            resume_cursor = next_cursor;
            if !visitor(entry)? {
                return Ok(DirectoryVisitProgress::Paused(resume_cursor));
            }
        }
        Ok(DirectoryVisitProgress::Complete)
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
        // Callers must classify the returned descriptor with fstat before
        // reading it. NONBLOCK prevents a path substituted with a FIFO or
        // device from pinning the blocking open worker before that check.
        self.open_existing(path, OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC)
            .await
    }

    pub(super) async fn open_write(&self, path: &Path) -> std::io::Result<tokio::fs::File> {
        self.open_existing(path, OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC)
            .await
    }

    /// Capture all security-relevant metadata before replacing a regular
    /// file. Hard-linked targets are refused because replacing one directory
    /// entry cannot preserve the inode identity of its other names.
    pub(super) async fn replacement_metadata(
        &self,
        path: &Path,
    ) -> std::io::Result<ReplacementTarget> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let relative = this.relative_path(&path)?;
            // First inspect through the already anchored parent directory.
            // A no-follow stat cannot block on or activate a FIFO, socket, or
            // device, and it also handles a self-referential final symlink
            // without asking openat2 to resolve that link.
            let inspected_identity = match this.replacement_identity_blocking(&path)? {
                ReplacementTargetIdentity::Missing => {
                    return Ok(ReplacementTarget {
                        identity: ReplacementTargetIdentity::Missing,
                        metadata: None,
                    });
                }
                ReplacementTargetIdentity::Existing(identity) => identity,
            };
            match inspected_identity.file_type {
                FileType::Symlink => {
                    return Ok(ReplacementTarget {
                        identity: ReplacementTargetIdentity::Existing(inspected_identity),
                        metadata: None,
                    });
                }
                FileType::RegularFile => {}
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "refusing to replace a non-regular filesystem object",
                    ));
                }
            }
            if inspected_identity.mode & 0o6000 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "refusing to preserve set-user-ID or set-group-ID mode bits while replacing a file",
                ));
            }

            // Xattrs require a regular data descriptor on Linux. Reopen the
            // path non-blocking and compare identity with the no-follow snapshot
            // so an external replacement between the two opens fails closed.
            let fd = openat2(
                &this.inner.root,
                relative,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                this.inner.resolve,
            )
            .map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("target changed while preserving metadata: {error}"),
                )
            })?;
            let opened = fstat(&fd).map_err(std::io::Error::from)?;
            if replacement_target_identity(&opened) != inspected_identity {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "target changed while preserving metadata",
                ));
            }
            if inspected_identity.links != 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "refusing to replace a hard-linked file because inode identity cannot be preserved",
                ));
            }
            let xattrs = read_all_xattrs(&fd)?;
            if xattrs
                .iter()
                .any(|(name, _)| is_privileged_xattr(name.as_c_str()))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "refusing to preserve a privileged extended attribute while replacing a file",
                ));
            }
            let captured = fstat(&fd).map_err(std::io::Error::from)?;
            if replacement_target_identity(&captured) != inspected_identity {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "target changed while preserving metadata",
                ));
            }
            Ok(ReplacementTarget {
                identity: ReplacementTargetIdentity::Existing(inspected_identity),
                metadata: Some(PreservedFileMetadata {
                    uid: captured.st_uid,
                    gid: captured.st_gid,
                    mode: captured.st_mode,
                    xattrs,
                }),
            })
        })
        .await
    }

    /// Inspect a destination without opening it for data access. This is used
    /// by upload preflight and overwrite-token validation; FIFOs, devices and
    /// sockets therefore cannot block the request worker.
    pub(super) async fn replacement_identity(
        &self,
        path: &Path,
    ) -> std::io::Result<ReplacementTargetIdentity> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || this.replacement_identity_blocking(&path)).await
    }

    fn replacement_identity_blocking(
        &self,
        path: &Path,
    ) -> std::io::Result<ReplacementTargetIdentity> {
        let parent = match self.open_parent_blocking(path, false) {
            Ok(parent) => parent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReplacementTargetIdentity::Missing);
            }
            Err(error) => return Err(error),
        };
        match statat(&parent.fd, &parent.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(ReplacementTargetIdentity::Existing(
                replacement_target_identity(&stat),
            )),
            Err(Errno::NOENT) => Ok(ReplacementTargetIdentity::Missing),
            Err(error) => Err(std::io::Error::from(error)),
        }
    }

    pub(super) async fn apply_replacement_metadata(
        &self,
        file: &tokio::fs::File,
        metadata: PreservedFileMetadata,
    ) -> std::io::Result<()> {
        let fd = dup(file).map_err(std::io::Error::from)?;
        run_blocking(move || apply_file_metadata(&fd, metadata)).await
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

    #[cfg(test)]
    pub(super) async fn create_new(&self, path: &Path) -> std::io::Result<(tokio::fs::File, bool)> {
        self.create_new_with_mode(path, Mode::from_raw_mode(0o666), false)
            .await
    }

    /// Create a new upload-internal file without ever making it accessible to
    /// group or other users. The explicit `fchmod` also neutralizes a
    /// permissive umask or inherited default ACL mode bits.
    pub(super) async fn create_private_new(
        &self,
        path: &Path,
    ) -> std::io::Result<(tokio::fs::File, bool)> {
        self.create_new_with_mode(path, Mode::from_raw_mode(0o600), true)
            .await
    }

    async fn create_new_with_mode(
        &self,
        path: &Path,
        mode: Mode,
        enforce_exact_mode: bool,
    ) -> std::io::Result<(tokio::fs::File, bool)> {
        let this = self.clone();
        let path = path.to_path_buf();
        let (file, created_ancestors) = run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let parent = this.open_parent_blocking(&path, true)?;
            let fd = match openat(
                &parent.fd,
                &parent.name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                mode,
            ) {
                Ok(fd) => fd,
                Err(error) => {
                    let error = std::io::Error::from(error);
                    let _ = this.rollback_created_ancestors_blocking(&parent.created_ancestors);
                    return Err(error);
                }
            };
            if enforce_exact_mode && let Err(error) = fchmod(&fd, mode) {
                let _ = unlinkat(&parent.fd, &parent.name, AtFlags::empty());
                let _ = this.rollback_created_ancestors_blocking(&parent.created_ancestors);
                return Err(std::io::Error::from(error));
            }
            Ok((File::from(fd), !parent.created_ancestors.is_empty()))
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
            if let Err(error) = mkdirat(&parent.fd, &parent.name, Mode::from_raw_mode(0o777)) {
                let error = std::io::Error::from(error);
                let _ = this.rollback_created_ancestors_blocking(&parent.created_ancestors);
                return Err(error);
            }
            fsync(&parent.fd).map_err(std::io::Error::from)?;
            Ok(!parent.created_ancestors.is_empty())
        })
        .await
    }

    pub(super) async fn ensure_parent(&self, path: &Path) -> std::io::Result<CreatedAncestors> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(CreatedAncestors {
                entries: this.open_parent_blocking(&path, true)?.created_ancestors,
            })
        })
        .await
    }

    pub(super) async fn rollback_created_ancestors(
        &self,
        created: CreatedAncestors,
    ) -> std::io::Result<()> {
        if created.entries.is_empty() {
            return Ok(());
        }
        let this = self.clone();
        run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            this.rollback_created_ancestors_blocking(&created.entries)
        })
        .await
    }

    /// Open the deepest existing directory on the way to `path` without
    /// creating anything. A fresh upload can reserve space on the correct
    /// filesystem before it materializes missing ancestors.
    pub(super) async fn open_nearest_existing_parent(
        &self,
        path: &Path,
    ) -> std::io::Result<tokio::fs::File> {
        let this = self.clone();
        let path = path.to_path_buf();
        let directory = run_blocking(move || {
            let relative = this.relative_path(&path)?;
            let relative_parent = relative.parent().unwrap_or_else(|| Path::new(""));
            let mut current = dup(&this.inner.root).map_err(std::io::Error::from)?;
            let mut prefix = PathBuf::new();
            for component in relative_parent.components() {
                let Component::Normal(component) = component else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path contains a non-normal component",
                    ));
                };
                prefix.push(component);
                match this.open_directory_from_root(&prefix) {
                    Ok(next) => current = next,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::NotADirectory
                                | std::io::ErrorKind::PermissionDenied
                        ) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(File::from(current))
        })
        .await?;
        Ok(tokio::fs::File::from_std(directory))
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

    pub(super) fn remove_entry_blocking(&self, path: &Path, is_dir: bool) -> std::io::Result<bool> {
        let parent = match self.open_parent_blocking(path, false) {
            Ok(parent) => parent,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        if is_dir {
            match remove_directory_at(&parent.fd, &parent.name) {
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

    /// Relocate a directory entry after rechecking the identities observed by
    /// the browser API. Missing destinations use `NOREPLACE` plus a pinned
    /// source post-check; this prevents a late target from being overwritten
    /// and refuses to report success if a different source object was moved.
    /// Existing-target replacement remains a check followed by ordinary
    /// rename, not a strict directory-entry CAS against external writers.
    pub(super) async fn rename_if_unchanged(
        &self,
        source: &Path,
        destination: &Path,
        expected_source: ReplacementTargetIdentity,
        expected_destination: Option<ReplacementTargetIdentity>,
    ) -> std::io::Result<CheckedRelocationOutcome> {
        let this = self.clone();
        let source = source.to_path_buf();
        let destination = destination.to_path_buf();
        run_blocking(move || {
            let _ancestor_creation = this
                .inner
                .ancestor_creation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let source = match this.open_parent_blocking(&source, false) {
                Ok(source) => source,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(CheckedRelocationOutcome::SourceChanged);
                }
                Err(error) => return Err(error),
            };
            let destination = match this.open_parent_blocking(&destination, false) {
                Ok(destination) => destination,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(CheckedRelocationOutcome::DestinationChanged);
                }
                Err(error) => return Err(error),
            };
            let same_parent = opened_directories_match(&source.fd, &destination.fd)?;
            let source_stat = match statat(&source.fd, &source.name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(source_stat) => source_stat,
                Err(Errno::NOENT | Errno::NOTDIR) => {
                    return Ok(CheckedRelocationOutcome::SourceChanged);
                }
                Err(error) => return Err(std::io::Error::from(error)),
            };
            if ReplacementTargetIdentity::Existing(replacement_target_identity(&source_stat))
                != expected_source
            {
                return Ok(CheckedRelocationOutcome::SourceChanged);
            }
            let current_destination = match statat(
                &destination.fd,
                &destination.name,
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(destination_stat) => ReplacementTargetIdentity::Existing(
                    replacement_target_identity(&destination_stat),
                ),
                Err(Errno::NOENT | Errno::NOTDIR) => ReplacementTargetIdentity::Missing,
                Err(error) => return Err(std::io::Error::from(error)),
            };

            if let Some(ReplacementTargetIdentity::Existing(destination_identity)) =
                expected_destination
            {
                if current_destination != ReplacementTargetIdentity::Existing(destination_identity)
                {
                    return Ok(CheckedRelocationOutcome::DestinationChanged);
                }
                if source_stat.st_dev == destination_identity.device
                    && source_stat.st_ino == destination_identity.inode
                {
                    return Ok(CheckedRelocationOutcome::SameFile);
                }
                renameat(&source.fd, &source.name, &destination.fd, &destination.name)
                    .map_err(std::io::Error::from)?;
            } else {
                let destination_changed = expected_destination.is_some();
                if current_destination != ReplacementTargetIdentity::Missing {
                    return Ok(if destination_changed {
                        CheckedRelocationOutcome::DestinationChanged
                    } else {
                        CheckedRelocationOutcome::DestinationExists
                    });
                }
                let source_anchor = match openat(
                    &source.fd,
                    &source.name,
                    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(anchor) => anchor,
                    Err(Errno::NOENT | Errno::NOTDIR) => {
                        return Ok(CheckedRelocationOutcome::SourceChanged);
                    }
                    Err(error) => return Err(std::io::Error::from(error)),
                };
                let anchored_source = fstat(&source_anchor).map_err(std::io::Error::from)?;
                if ReplacementTargetIdentity::Existing(replacement_target_identity(
                    &anchored_source,
                )) != expected_source
                {
                    return Ok(CheckedRelocationOutcome::SourceChanged);
                }

                this.run_before_missing_rename_hook();
                match rename_missing_and_verify(
                    &source.fd,
                    &source.name,
                    &source_anchor,
                    &destination.fd,
                    &destination.name,
                )? {
                    CheckedMissingRename::Renamed => {}
                    CheckedMissingRename::DestinationExists => {
                        return Ok(if destination_changed {
                            CheckedRelocationOutcome::DestinationChanged
                        } else {
                            CheckedRelocationOutcome::DestinationExists
                        });
                    }
                    CheckedMissingRename::PublishedIdentityUnknown(error) => {
                        let _ = sync_renamed_parents(&source.fd, &destination.fd, same_parent);
                        return Err(error);
                    }
                }
            }
            sync_renamed_parents(&source.fd, &destination.fd, same_parent)?;
            Ok(CheckedRelocationOutcome::Relocated)
        })
        .await
    }

    /// Publish an already-opened staging file after rechecking both directory
    /// entries. A target expected to be missing is committed with `NOREPLACE`
    /// and the resulting name is checked against the open staging descriptor.
    /// Existing-target replacement remains a check followed by ordinary
    /// rename, not a strict directory-entry CAS against external writers.
    pub(super) async fn rename_replace_if_unchanged(
        &self,
        source: &Path,
        source_file: &tokio::fs::File,
        destination: &Path,
        expected_destination: ReplacementTargetIdentity,
    ) -> ReplaceAndSyncOutcome {
        let this = self.clone();
        let source = source.to_path_buf();
        let destination = destination.to_path_buf();
        let opened_source = match dup(source_file) {
            Ok(opened_source) => opened_source,
            Err(error) => {
                return ReplaceAndSyncOutcome::NotPublished(std::io::Error::from(error));
            }
        };
        let result = blocking_io_gate()
            .run(move || {
                let _ancestor_creation = this
                    .inner
                    .ancestor_creation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let prepared = (|| -> std::io::Result<PreparedCheckedReplace> {
                    let source = this.open_parent_blocking(&source, false)?;
                    let destination = this.open_parent_blocking(&destination, false)?;
                    let same_parent = opened_directories_match(&source.fd, &destination.fd)?;

                    let opened_source_stat = fstat(&opened_source).map_err(std::io::Error::from)?;
                    let named_source_stat =
                        match statat(&source.fd, &source.name, AtFlags::SYMLINK_NOFOLLOW) {
                            Ok(stat) => stat,
                            Err(Errno::NOENT) => return Ok(PreparedCheckedReplace::Rejected),
                            Err(error) => return Err(std::io::Error::from(error)),
                        };
                    let opened_source_identity = replacement_target_identity(&opened_source_stat);
                    let named_source_identity = replacement_target_identity(&named_source_stat);
                    if opened_source_identity != named_source_identity
                        || opened_source_identity.file_type != FileType::RegularFile
                        || opened_source_identity.links != 1
                    {
                        return Ok(PreparedCheckedReplace::Rejected);
                    }

                    let current_destination = match statat(
                        &destination.fd,
                        &destination.name,
                        AtFlags::SYMLINK_NOFOLLOW,
                    ) {
                        Ok(stat) => Some(replacement_target_identity(&stat)),
                        Err(Errno::NOENT) => None,
                        Err(error) => return Err(std::io::Error::from(error)),
                    };
                    match expected_destination {
                        ReplacementTargetIdentity::Missing => {
                            if current_destination.is_some() {
                                return Ok(PreparedCheckedReplace::Rejected);
                            }
                            this.run_before_missing_rename_hook();
                            match rename_missing_and_verify(
                                &source.fd,
                                &source.name,
                                &opened_source,
                                &destination.fd,
                                &destination.name,
                            )? {
                                CheckedMissingRename::Renamed => {}
                                CheckedMissingRename::DestinationExists => {
                                    return Ok(PreparedCheckedReplace::Rejected);
                                }
                                CheckedMissingRename::PublishedIdentityUnknown(error) => {
                                    let _ = sync_renamed_parents(
                                        &source.fd,
                                        &destination.fd,
                                        same_parent,
                                    );
                                    return Ok(PreparedCheckedReplace::PublishedIdentityUnknown(
                                        error,
                                    ));
                                }
                            }
                        }
                        ReplacementTargetIdentity::Existing(expected) => {
                            if current_destination != Some(expected) {
                                return Ok(PreparedCheckedReplace::Rejected);
                            }
                            renameat(&source.fd, &source.name, &destination.fd, &destination.name)
                                .map_err(std::io::Error::from)?;
                        }
                    }
                    Ok(PreparedCheckedReplace::Renamed(
                        source,
                        destination,
                        same_parent,
                    ))
                })();

                let (source, destination, same_parent) = match prepared {
                    Ok(PreparedCheckedReplace::Renamed(source, destination, same_parent)) => {
                        (source, destination, same_parent)
                    }
                    Ok(PreparedCheckedReplace::Rejected) => {
                        return ReplaceAndSyncOutcome::Rejected;
                    }
                    Ok(PreparedCheckedReplace::PublishedIdentityUnknown(error)) => {
                        return ReplaceAndSyncOutcome::PublishedDurabilityUnknown(error);
                    }
                    Err(error) => return ReplaceAndSyncOutcome::NotPublished(error),
                };
                if let Err(error) = sync_renamed_parents(&source.fd, &destination.fd, same_parent) {
                    return ReplaceAndSyncOutcome::PublishedDurabilityUnknown(error);
                }
                ReplaceAndSyncOutcome::Published
            })
            .await;
        match result {
            Ok(outcome) => outcome,
            Err(error) => ReplaceAndSyncOutcome::PublishedDurabilityUnknown(error),
        }
    }

    #[cfg(test)]
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
            let destination = this.open_parent_blocking(&destination, false)?;
            let same_parent = opened_directories_match(&source.fd, &destination.fd)?;
            match renameat_with(
                &source.fd,
                &source.name,
                &destination.fd,
                &destination.name,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => {
                    sync_renamed_parents(&source.fd, &destination.fd, same_parent)?;
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

    #[cfg(test)]
    pub(super) async fn move_to_trash(&self, path: &Path) -> std::io::Result<TrashEntry> {
        for _ in 0..16 {
            match self.move_to_trash_with_id(path, Uuid::new_v4()).await {
                Ok(entry) => return Ok(entry),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "failed to allocate a unique internal trash name",
        ))
    }

    /// Atomically hides `path` under a caller-selected, collision-resistant
    /// trash identifier.
    ///
    /// The deterministic identifier lets the durable control plane record an
    /// intent before the filesystem rename. A restart can then reconcile that
    /// exact directory entry instead of rediscovering it with an unbounded
    /// tree scan.
    #[cfg(test)]
    pub(super) async fn move_to_trash_with_id(
        &self,
        path: &Path,
        trash_id: Uuid,
    ) -> std::io::Result<TrashEntry> {
        self.move_to_trash_with_id_checked(path, trash_id, None)
            .await
    }

    pub(super) async fn delete_identity(&self, path: &Path) -> std::io::Result<DeleteIdentity> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            let parent = this.open_parent_blocking(&path, false)?;
            let metadata = statat(&parent.fd, &parent.name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            Ok(delete_identity_from_stat(&metadata))
        })
        .await
    }

    #[cfg(test)]
    pub(super) async fn move_to_trash_with_expected_identity(
        &self,
        path: &Path,
        trash_id: Uuid,
        expected: DeleteIdentity,
    ) -> std::io::Result<TrashEntry> {
        match self
            .move_to_trash_with_expected_identity_outcome(path, trash_id, expected, None)
            .await
        {
            CheckedTrashMove::Moved(entry) => Ok(entry),
            CheckedTrashMove::TargetChanged => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "delete target identity changed before the commit boundary",
            )),
            CheckedTrashMove::NotMoved(error) | CheckedTrashMove::DurabilityUnknown(error) => {
                Err(error)
            }
        }
    }

    /// Classify the checked rename around its filesystem commit boundary.
    /// Callers may discard a durable intent only for `TargetChanged` or
    /// `NotMoved`; an fsync failure after rename must remain recoverable.
    pub(super) async fn move_to_trash_with_expected_identity_outcome(
        &self,
        path: &Path,
        trash_id: Uuid,
        expected: DeleteIdentity,
        expected_revision_identity: Option<ReplacementTargetIdentity>,
    ) -> CheckedTrashMove {
        let this = self.clone();
        let path = path.to_path_buf();
        let trash_path = match self.trash_path_for_id(&path, trash_id) {
            Ok(path) => path,
            Err(error) => return CheckedTrashMove::NotMoved(error),
        };
        let name = trash_path
            .file_name()
            .expect("a validated trash path has a file name")
            .to_os_string();
        match blocking_io_gate()
            .run(move || {
                let parent = match this.open_parent_blocking(&path, false) {
                    Ok(parent) => parent,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return CheckedTrashMove::TargetChanged;
                    }
                    Err(error) => return CheckedTrashMove::NotMoved(error),
                };
                let metadata = match statat(&parent.fd, &parent.name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(metadata) => metadata,
                    Err(Errno::NOENT | Errno::NOTDIR) => {
                        return CheckedTrashMove::TargetChanged;
                    }
                    Err(error) => {
                        return CheckedTrashMove::NotMoved(std::io::Error::from(error));
                    }
                };
                let actual = delete_identity_from_stat(&metadata);
                let actual_revision_identity =
                    ReplacementTargetIdentity::Existing(replacement_target_identity(&metadata));
                if actual != expected
                    || expected_revision_identity
                        .is_some_and(|expected| expected != actual_revision_identity)
                {
                    return CheckedTrashMove::TargetChanged;
                }
                let root_anchor = match openat(
                    &parent.fd,
                    &parent.name,
                    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(anchor) => anchor,
                    Err(Errno::NOENT | Errno::NOTDIR) => {
                        return CheckedTrashMove::TargetChanged;
                    }
                    Err(error) => {
                        return CheckedTrashMove::NotMoved(std::io::Error::from(error));
                    }
                };
                let opened_revision_identity = match fstat(&root_anchor) {
                    Ok(metadata) => ReplacementTargetIdentity::Existing(
                        replacement_target_identity(&metadata),
                    ),
                    Err(error) => {
                        return CheckedTrashMove::NotMoved(std::io::Error::from(error));
                    }
                };
                let named_revision_identity = match statat(
                    &parent.fd,
                    &parent.name,
                    AtFlags::SYMLINK_NOFOLLOW,
                ) {
                    Ok(metadata) => ReplacementTargetIdentity::Existing(
                        replacement_target_identity(&metadata),
                    ),
                    Err(Errno::NOENT | Errno::NOTDIR) => {
                        return CheckedTrashMove::TargetChanged;
                    }
                    Err(error) => {
                        return CheckedTrashMove::NotMoved(std::io::Error::from(error));
                    }
                };
                if opened_revision_identity != actual_revision_identity
                    || named_revision_identity != opened_revision_identity
                {
                    return CheckedTrashMove::TargetChanged;
                }
                if let Err(error) = renameat_with(
                    &parent.fd,
                    &parent.name,
                    &parent.fd,
                    &name,
                    RenameFlags::NOREPLACE,
                ) {
                    return classify_failed_checked_trash_rename(
                        &parent.fd,
                        &parent.name,
                        &name,
                        expected,
                        std::io::Error::from(error),
                    );
                }
                let anchored_metadata = match fstat(&root_anchor) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        let _ = fsync(&parent.fd);
                        return CheckedTrashMove::DurabilityUnknown(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "renamed trash anchor could not be verified at the commit boundary: {error}"
                            ),
                        ));
                    }
                };
                let moved_metadata = match statat(&parent.fd, &name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        let _ = fsync(&parent.fd);
                        return CheckedTrashMove::DurabilityUnknown(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "renamed trash entry could not be verified at the commit boundary: {error}"
                            ),
                        ));
                    }
                };
                let moved_identity = delete_identity_from_stat(&moved_metadata);
                let anchored_revision_identity = ReplacementTargetIdentity::Existing(
                    replacement_target_identity(&anchored_metadata),
                );
                let moved_revision_identity = ReplacementTargetIdentity::Existing(
                    replacement_target_identity(&moved_metadata),
                );
                if moved_identity != expected {
                    let _ = fsync(&parent.fd);
                    return CheckedTrashMove::DurabilityUnknown(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "renamed trash entry identity changed at the commit boundary",
                    ));
                }
                if moved_revision_identity != anchored_revision_identity {
                    let _ = fsync(&parent.fd);
                    return CheckedTrashMove::DurabilityUnknown(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "renamed trash entry no longer matches its pinned commit identity",
                    ));
                }
                let trash_revision = anchored_revision_identity.purge_revision();
                if let Err(error) = fsync(&parent.fd) {
                    return CheckedTrashMove::DurabilityUnknown(std::io::Error::from(error));
                }
                CheckedTrashMove::Moved(TrashEntry::new(
                    File::from(parent.fd),
                    name,
                    moved_identity,
                    root_anchor,
                    trash_revision,
                ))
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => CheckedTrashMove::DurabilityUnknown(error),
        }
    }

    #[cfg(test)]
    async fn move_to_trash_with_id_checked(
        &self,
        path: &Path,
        trash_id: Uuid,
        expected: Option<DeleteIdentity>,
    ) -> std::io::Result<TrashEntry> {
        let this = self.clone();
        let path = path.to_path_buf();
        let trash_path = self.trash_path_for_id(&path, trash_id)?;
        let name = trash_path
            .file_name()
            .expect("a validated trash path has a file name")
            .to_os_string();
        run_blocking(move || {
            let parent = this.open_parent_blocking(&path, false)?;
            let metadata = statat(&parent.fd, &parent.name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let actual = delete_identity_from_stat(&metadata);
            if expected.is_some_and(|expected| expected != actual) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "delete target identity changed before the commit boundary",
                ));
            }
            let root_anchor = openat(
                &parent.fd,
                &parent.name,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            let opened_identity = ReplacementTargetIdentity::Existing(replacement_target_identity(
                &fstat(&root_anchor).map_err(std::io::Error::from)?,
            ));
            if opened_identity
                != ReplacementTargetIdentity::Existing(replacement_target_identity(&metadata))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "delete target identity changed while pinning it",
                ));
            }
            renameat_with(
                &parent.fd,
                &parent.name,
                &parent.fd,
                &name,
                RenameFlags::NOREPLACE,
            )
            .map_err(std::io::Error::from)?;
            let moved_metadata = statat(&parent.fd, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let anchored_identity = ReplacementTargetIdentity::Existing(
                replacement_target_identity(&fstat(&root_anchor).map_err(std::io::Error::from)?),
            );
            if ReplacementTargetIdentity::Existing(replacement_target_identity(&moved_metadata))
                != anchored_identity
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "renamed trash entry no longer matches its pinned commit identity",
                ));
            }
            fsync(&parent.fd).map_err(std::io::Error::from)?;
            Ok(TrashEntry::new(
                File::from(parent.fd),
                name,
                actual,
                root_anchor,
                anchored_identity.purge_revision(),
            ))
        })
        .await
    }

    pub(super) fn trash_path_for_id(
        &self,
        path: &Path,
        trash_id: Uuid,
    ) -> std::io::Result<PathBuf> {
        let _ = self.relative_path(path)?;
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "trash target has no parent directory",
            )
        })?;
        Ok(parent.join(delete_trash_name(trash_id)))
    }

    /// Moves an identity-ambiguous internal trash entry to a reserved name
    /// that maintenance will never purge automatically. This releases the
    /// durable job capacity without turning an unrelated occupant into an
    /// orphan-trash deletion candidate.
    pub(super) async fn quarantine_internal_trash(
        &self,
        path: &Path,
    ) -> std::io::Result<Option<PathBuf>> {
        let this = self.clone();
        let path = path.to_path_buf();
        blocking_io_gate()
            .run(move || {
                let file_name = path.file_name().and_then(|name| name.to_str());
                if file_name.and_then(classify_internal_name)
                    != Some(InternalEntryName::DeleteTrash)
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "only a canonical internal trash entry can be quarantined",
                    ));
                }
                let parent_path = path
                    .parent()
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "internal trash entry has no parent directory",
                        )
                    })?
                    .to_path_buf();
                let parent = this.open_parent_blocking(&path, false)?;
                for _ in 0..16 {
                    let name = OsString::from(quarantine_name(Uuid::new_v4()));
                    match renameat_with(
                        &parent.fd,
                        &parent.name,
                        &parent.fd,
                        &name,
                        RenameFlags::NOREPLACE,
                    ) {
                        Ok(()) => {
                            fsync(&parent.fd).map_err(std::io::Error::from)?;
                            return Ok(Some(parent_path.join(name)));
                        }
                        Err(Errno::EXIST) => continue,
                        Err(Errno::NOENT | Errno::NOTDIR) => return Ok(None),
                        Err(error) => return Err(std::io::Error::from(error)),
                    }
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "failed to allocate a unique quarantine name",
                ))
            })
            .await
            .map_err(std::io::Error::other)?
    }

    pub(super) fn capture_entry_for_purge_blocking(
        &self,
        path: &Path,
        expected_directory: bool,
    ) -> std::io::Result<Option<TrashEntry>> {
        self.capture_entry_for_purge_with_type_blocking(path, Some(expected_directory), None)
    }

    pub(super) async fn capture_entry_for_purge(
        &self,
        path: &Path,
        expected_directory: bool,
    ) -> std::io::Result<Option<TrashEntry>> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            this.capture_entry_for_purge_with_type_blocking(&path, Some(expected_directory), None)
        })
        .await
    }

    pub(super) async fn capture_entry_for_purge_with_revision(
        &self,
        path: &Path,
        expected_directory: bool,
        expected_revision: [u8; 32],
    ) -> std::io::Result<Option<TrashEntry>> {
        let this = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || {
            this.capture_entry_for_purge_with_type_blocking(
                &path,
                Some(expected_directory),
                Some(expected_revision),
            )
        })
        .await
    }

    /// Reopens an exact hidden trash entry when its type was not durably
    /// recorded before a crash. The caller still receives an fd-relative,
    /// no-follow purge capability; no pathname is trusted after this point.
    #[cfg(test)]
    pub(super) fn capture_any_entry_for_purge_blocking(
        &self,
        path: &Path,
    ) -> std::io::Result<Option<TrashEntry>> {
        self.capture_entry_for_purge_with_type_blocking(path, None, None)
    }

    fn capture_entry_for_purge_with_type_blocking(
        &self,
        path: &Path,
        expected_directory: Option<bool>,
        expected_revision: Option<[u8; 32]>,
    ) -> std::io::Result<Option<TrashEntry>> {
        let parent = match self.open_parent_blocking(path, false) {
            Ok(parent) => parent,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let root_anchor = match openat(
            &parent.fd,
            &parent.name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(anchor) => anchor,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(std::io::Error::from(error)),
        };
        let opened_metadata = fstat(&root_anchor).map_err(std::io::Error::from)?;
        let named_metadata = match statat(&parent.fd, &parent.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(std::io::Error::from(error)),
        };
        let opened_revision_identity =
            ReplacementTargetIdentity::Existing(replacement_target_identity(&opened_metadata));
        let named_revision_identity =
            ReplacementTargetIdentity::Existing(replacement_target_identity(&named_metadata));
        if opened_revision_identity != named_revision_identity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "purge candidate identity changed while pinning it",
            ));
        }
        let identity = delete_identity_from_stat(&opened_metadata);
        if expected_directory.is_some_and(|expected| identity.is_directory != expected) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "purge candidate changed type during maintenance discovery",
            ));
        }
        let trash_revision = opened_revision_identity.purge_revision();
        if expected_revision.is_some_and(|expected| trash_revision != expected) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "purge candidate revision does not match the durable trash revision",
            ));
        }
        Ok(Some(TrashEntry::new(
            File::from(parent.fd),
            parent.name,
            identity,
            root_anchor,
            trash_revision,
        )))
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
        let mut created_ancestors = Vec::new();

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
                    let created_here =
                        match mkdirat(&current, component, Mode::from_raw_mode(0o777)) {
                            Ok(()) => {
                                if let Err(error) = fsync(&current) {
                                    let _ = unlinkat(&current, component, AtFlags::REMOVEDIR);
                                    let _ = self
                                        .rollback_created_ancestors_blocking(&created_ancestors);
                                    return Err(std::io::Error::from(error));
                                }
                                true
                            }
                            // Another sibling mutation may have created this
                            // ancestor after openat2 reported ENOENT. It is not
                            // enough to rely on that request's later fsync: this
                            // request can finish first, so it must independently
                            // make the shared ancestor entry durable.
                            Err(Errno::EXIST) => {
                                if let Err(error) = fsync(&current) {
                                    let _ = self
                                        .rollback_created_ancestors_blocking(&created_ancestors);
                                    return Err(std::io::Error::from(error));
                                }
                                false
                            }
                            Err(err) => {
                                let _ =
                                    self.rollback_created_ancestors_blocking(&created_ancestors);
                                return Err(std::io::Error::from(err));
                            }
                        };
                    let created_identity = if created_here {
                        match statat(&current, component, AtFlags::SYMLINK_NOFOLLOW) {
                            Ok(metadata)
                                if FileType::from_raw_mode(metadata.st_mode)
                                    == FileType::Directory =>
                            {
                                Some(FileIdentity {
                                    device: metadata.st_dev,
                                    inode: metadata.st_ino,
                                })
                            }
                            Ok(_) => {
                                let _ = unlinkat(&current, component, AtFlags::REMOVEDIR);
                                let _ =
                                    self.rollback_created_ancestors_blocking(&created_ancestors);
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "newly created ancestor changed type",
                                ));
                            }
                            Err(error) => {
                                let _ = unlinkat(&current, component, AtFlags::REMOVEDIR);
                                let _ =
                                    self.rollback_created_ancestors_blocking(&created_ancestors);
                                return Err(std::io::Error::from(error));
                            }
                        }
                    } else {
                        None
                    };
                    current = match self.open_directory_from_root(&prefix) {
                        Ok(current) => current,
                        Err(error) => {
                            if created_here
                                && let Ok(parent) = self.open_directory_from_root(
                                    prefix.parent().unwrap_or_else(|| Path::new("")),
                                )
                            {
                                let _ = unlinkat(&parent, component, AtFlags::REMOVEDIR);
                                let _ = fsync(&parent);
                            }
                            let _ = self.rollback_created_ancestors_blocking(&created_ancestors);
                            return Err(error);
                        }
                    };
                    if let Some(identity) = created_identity {
                        created_ancestors.push(CreatedAncestor {
                            path: self.inner.root_path.join(&prefix),
                            identity,
                        });
                    }
                }
                Err(err) => {
                    let _ = self.rollback_created_ancestors_blocking(&created_ancestors);
                    return Err(err);
                }
            }
        }

        Ok(OpenedParent {
            fd: current,
            name,
            created_ancestors,
        })
    }

    fn rollback_created_ancestors_blocking(
        &self,
        created: &[CreatedAncestor],
    ) -> std::io::Result<()> {
        for ancestor in created.iter().rev() {
            let parent = match self.open_parent_blocking(&ancestor.path, false) {
                Ok(parent) => parent,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let current = match statat(&parent.fd, &parent.name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(current) => current,
                Err(Errno::NOENT) => continue,
                Err(error) => return Err(std::io::Error::from(error)),
            };
            let current_identity = FileIdentity {
                device: current.st_dev,
                inode: current.st_ino,
            };
            if FileType::from_raw_mode(current.st_mode) != FileType::Directory
                || current_identity != ancestor.identity
            {
                break;
            }
            match unlinkat(&parent.fd, &parent.name, AtFlags::REMOVEDIR) {
                Ok(()) => fsync(&parent.fd).map_err(std::io::Error::from)?,
                Err(Errno::NOENT) => {}
                Err(Errno::NOTEMPTY | Errno::EXIST) => break,
                Err(error) => return Err(std::io::Error::from(error)),
            }
        }
        Ok(())
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

#[inline]
#[cfg(test)]
fn directory_entry_is_symlink<F>(file_type: FileType, probe: F) -> std::io::Result<bool>
where
    F: FnOnce() -> std::io::Result<bool>,
{
    match file_type {
        FileType::Symlink => Ok(true),
        FileType::Unknown => probe(),
        _ => Ok(false),
    }
}

fn opened_directories_match<F: AsFd, G: AsFd>(first: &F, second: &G) -> std::io::Result<bool> {
    let first = fstat(first).map_err(std::io::Error::from)?;
    let second = fstat(second).map_err(std::io::Error::from)?;
    Ok(first.st_dev == second.st_dev && first.st_ino == second.st_ino)
}

fn sync_renamed_parents<F: AsFd, G: AsFd>(
    source: &F,
    destination: &G,
    same_parent: bool,
) -> std::io::Result<()> {
    fsync(source).map_err(std::io::Error::from)?;
    if !same_parent {
        fsync(destination).map_err(std::io::Error::from)?;
    }
    Ok(())
}

fn delete_identity_from_stat(stat: &rustix::fs::Stat) -> DeleteIdentity {
    DeleteIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
        is_directory: FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
    }
}

/// Recheck both names after a failed rename before claiming that no filesystem
/// mutation occurred. Some remote filesystems can complete a rename but report
/// an I/O or transport error to the client. Only the exact original source
/// still present together with a successfully inspected, non-matching trash
/// name proves that the checked rename did not consume the source.
fn classify_failed_checked_trash_rename<F: AsFd>(
    parent: &F,
    source_name: &OsString,
    trash_name: &OsString,
    expected: DeleteIdentity,
    rename_error: std::io::Error,
) -> CheckedTrashMove {
    let source = delete_identity_at(parent, source_name);
    let trash = delete_identity_at(parent, trash_name);
    if matches!(source, Ok(Some(identity)) if identity == expected)
        && matches!(trash, Ok(identity) if identity != Some(expected))
    {
        CheckedTrashMove::NotMoved(rename_error)
    } else {
        CheckedTrashMove::DurabilityUnknown(rename_error)
    }
}

fn delete_identity_at<F: AsFd>(
    parent: &F,
    name: &OsString,
) -> std::io::Result<Option<DeleteIdentity>> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => Ok(Some(delete_identity_from_stat(&metadata))),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(std::io::Error::from(error)),
    }
}

/// Move into a destination that was observed missing without ever replacing a
/// late external occupant. A successful rename is still not a kernel CAS on
/// the source pathname, so prove that the destination now names the pinned
/// source object before reporting success. Failure after the rename is
/// deliberately classified as uncertain: moving or deleting either pathname
/// at that point could affect an object installed by an external writer.
fn rename_missing_and_verify<S, A, D>(
    source_parent: &S,
    source_name: &OsString,
    source_anchor: &A,
    destination_parent: &D,
    destination_name: &OsString,
) -> std::io::Result<CheckedMissingRename>
where
    S: AsFd,
    A: AsFd,
    D: AsFd,
{
    match renameat_with(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {}
        Err(Errno::EXIST) => return Ok(CheckedMissingRename::DestinationExists),
        Err(error) => return Err(std::io::Error::from(error)),
    }

    let anchored_source = match fstat(source_anchor) {
        Ok(stat) => replacement_target_identity(&stat),
        Err(error) => {
            return Ok(CheckedMissingRename::PublishedIdentityUnknown(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("renamed source anchor could not be verified: {error}"),
                ),
            ));
        }
    };
    let named_destination = match statat(
        destination_parent,
        destination_name,
        AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(stat) => replacement_target_identity(&stat),
        Err(error) => {
            return Ok(CheckedMissingRename::PublishedIdentityUnknown(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("renamed destination could not be verified: {error}"),
                ),
            ));
        }
    };
    if anchored_source != named_destination {
        return Ok(CheckedMissingRename::PublishedIdentityUnknown(
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "renamed destination no longer matches the pinned source identity",
            ),
        ));
    }
    Ok(CheckedMissingRename::Renamed)
}

fn replacement_target_identity(stat: &rustix::fs::Stat) -> ExistingReplacementTarget {
    ExistingReplacementTarget {
        device: stat.st_dev,
        inode: stat.st_ino,
        file_type: FileType::from_raw_mode(stat.st_mode),
        links: stat.st_nlink,
        size: stat.st_size,
        uid: stat.st_uid,
        gid: stat.st_gid,
        mode: stat.st_mode,
        modified_seconds: stat.st_mtime,
        modified_nanoseconds: stat.st_mtime_nsec,
        changed_seconds: stat.st_ctime,
        changed_nanoseconds: stat.st_ctime_nsec,
    }
}

const fn file_type_revision_tag(file_type: FileType) -> [u8; 1] {
    [match file_type {
        FileType::RegularFile => 1,
        FileType::Directory => 2,
        FileType::Symlink => 3,
        FileType::BlockDevice => 4,
        FileType::CharacterDevice => 5,
        FileType::Fifo => 6,
        FileType::Socket => 7,
        FileType::Unknown => 255,
    }]
}

const XATTR_LIST_LIMIT: usize = 64 * 1024;
const XATTR_VALUE_LIMIT: usize = 64 * 1024;
const XATTR_TOTAL_LIMIT: usize = 1024 * 1024;
const XATTR_COUNT_LIMIT: usize = 1024;

fn read_all_xattrs<F: AsFd>(file: F) -> std::io::Result<Vec<(CString, Vec<u8>)>> {
    let mut names = vec![0_u8; XATTR_LIST_LIMIT];
    let names_len = flistxattr(&file, &mut names).map_err(std::io::Error::from)?;
    names.truncate(names_len);
    let name_count = names.iter().filter(|byte| **byte == 0).count();
    if name_count > XATTR_COUNT_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "target has too many extended attributes to preserve",
        ));
    }
    let mut attributes = Vec::new();
    attributes.try_reserve_exact(name_count).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            "unable to allocate the extended-attribute index",
        )
    })?;
    let mut total = attributes
        .capacity()
        .saturating_mul(std::mem::size_of::<(CString, Vec<u8>)>());
    for raw_name in names.split_inclusive(|byte| *byte == 0) {
        if raw_name.is_empty() {
            continue;
        }
        if raw_name.last() != Some(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "filesystem returned an unterminated extended-attribute name",
            ));
        }
        let name = CStr::from_bytes_with_nul(raw_name)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "filesystem returned an invalid extended-attribute name",
                )
            })?
            .to_owned();
        let mut empty_value = [0_u8; 0];
        let value_len =
            fgetxattr(&file, name.as_c_str(), &mut empty_value).map_err(std::io::Error::from)?;
        if value_len > XATTR_VALUE_LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "target extended-attribute value exceeds the preservation budget",
            ));
        }
        let entry_bytes = name.as_bytes_with_nul().len().saturating_add(value_len);
        total = total.checked_add(entry_bytes).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "extended-attribute size overflow",
            )
        })?;
        if total > XATTR_TOTAL_LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "target extended attributes exceed the preservation budget",
            ));
        }
        let mut value = vec![0_u8; value_len];
        let value_len =
            fgetxattr(&file, name.as_c_str(), &mut value).map_err(std::io::Error::from)?;
        value.truncate(value_len);
        attributes.push((name, value));
    }
    Ok(attributes)
}

fn is_privileged_xattr(name: &CStr) -> bool {
    let name = name.to_bytes();
    name.starts_with(b"security.") || name.starts_with(b"trusted.")
}

fn is_posix_access_acl(name: &CStr) -> bool {
    name.to_bytes() == b"system.posix_acl_access"
}

fn apply_file_metadata<F: AsFd>(file: F, metadata: PreservedFileMetadata) -> std::io::Result<()> {
    let current = fstat(&file).map_err(std::io::Error::from)?;
    if current.st_uid != metadata.uid || current.st_gid != metadata.gid {
        fchown(
            &file,
            Some(Uid::from_raw(metadata.uid)),
            Some(Gid::from_raw(metadata.gid)),
        )
        .map_err(std::io::Error::from)?;
    }
    // A newly staged inode can inherit default ACLs or security labels from
    // its parent. Remove every attribute absent from the replaced target
    // before restoring that target's exact attribute set.
    for (name, _) in read_all_xattrs(&file)? {
        if !metadata
            .xattrs
            .iter()
            .any(|(preserved_name, _)| preserved_name == &name)
        {
            fremovexattr(&file, name.as_c_str()).map_err(std::io::Error::from)?;
        }
    }
    // flistxattr does not define an ordering. Restore ordinary attributes
    // before the POSIX access ACL because installing a restrictive ACL can
    // immediately remove the owner's write permission and make a later
    // user.* update fail with EACCES.
    for (name, value) in metadata
        .xattrs
        .iter()
        .filter(|(name, _)| !is_posix_access_acl(name.as_c_str()))
    {
        fsetxattr(&file, name.as_c_str(), value, XattrFlags::empty())
            .map_err(std::io::Error::from)?;
    }
    if let Some((name, value)) = metadata
        .xattrs
        .iter()
        .find(|(name, _)| is_posix_access_acl(name.as_c_str()))
    {
        fsetxattr(&file, name.as_c_str(), value, XattrFlags::empty())
            .map_err(std::io::Error::from)?;
    }
    // Apply the final mode only after xattrs. In particular, Linux refuses to
    // change user.* attributes once an unprivileged owner has lost write
    // permission, so an early restrictive chmod would make preservation of a
    // read-only target fail partway through metadata replay.
    fchmod(&file, Mode::from_raw_mode(metadata.mode & 0o7777)).map_err(std::io::Error::from)?;
    Ok(())
}

async fn run_blocking<T, F>(task: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    blocking_io_gate().run_io(task).await
}

#[cfg(test)]
mod tests;
