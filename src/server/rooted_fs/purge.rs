use super::{
    DeleteIdentity, DirectoryCursor, ExistingReplacementTarget, ReplacementTargetIdentity,
    replacement_target_identity,
};
use crate::server::blocking_io::blocking_io_gate;
use rustix::{
    fd::OwnedFd,
    fs::{
        AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags, fstat, fsync, openat, openat2, statat,
        unlinkat,
    },
    io::{Errno, dup},
};
use std::{
    ffi::OsString,
    fmt,
    fs::File,
    os::{
        fd::AsFd,
        unix::ffi::{OsStrExt, OsStringExt},
    },
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(in crate::server) struct TrashEntry {
    parent: File,
    pub(super) name: OsString,
    is_dir: bool,
    identity: DeleteIdentity,
    root_anchor: OwnedFd,
    trash_revision: [u8; 32],
    pub(super) purge_stack: Vec<PurgeDirectory>,
    pub(super) purge_resume: Option<PurgeResume>,
    pub(super) pending_unlink: Option<PurgeDirectory>,
    #[cfg(test)]
    pause_after_pending_unlink: bool,
    #[cfg(test)]
    inject_entry_before_directory_unlink: bool,
}

pub(super) struct PurgeDirectory {
    name: OsString,
    cursor: DirectoryCursor,
    identity: DeleteIdentity,
    revision_identity: ExistingReplacementTarget,
    pending_anchor: Option<OwnedFd>,
}

pub(super) struct PurgeResume {
    directory: OwnedFd,
    pub(super) depth: usize,
}

pub(in crate::server) enum TrashPurgeProgress {
    Complete,
    Pending(TrashEntry),
}

pub(in crate::server) struct TrashPurgeError {
    entry: TrashEntry,
    source: std::io::Error,
}

impl TrashPurgeError {
    pub(in crate::server) fn into_parts(self) -> (TrashEntry, std::io::Error) {
        (self.entry, self.source)
    }
}

impl fmt::Display for TrashPurgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl fmt::Debug for TrashPurgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrashPurgeError")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl std::error::Error for TrashPurgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl TrashEntry {
    pub(super) fn new(
        parent: File,
        name: OsString,
        identity: DeleteIdentity,
        root_anchor: OwnedFd,
        trash_revision: [u8; 32],
    ) -> Self {
        Self {
            parent,
            name,
            is_dir: identity.is_directory,
            identity,
            root_anchor,
            trash_revision,
            purge_stack: Vec::new(),
            purge_resume: None,
            pending_unlink: None,
            #[cfg(test)]
            pause_after_pending_unlink: false,
            #[cfg(test)]
            inject_entry_before_directory_unlink: false,
        }
    }

    pub(in crate::server) fn identity(&self) -> DeleteIdentity {
        self.identity
    }

    pub(in crate::server) fn trash_revision(&self) -> [u8; 32] {
        self.trash_revision
    }

    pub(in crate::server) async fn purge_slice(
        self,
        max_entries: usize,
        max_duration: Duration,
        cancellation: CancellationToken,
    ) -> std::result::Result<TrashPurgeProgress, TrashPurgeError> {
        let entry = Arc::new(Mutex::new(self));
        let worker_entry = entry.clone();
        let result = blocking_io_gate()
            .run(move || {
                let deadline = Instant::now()
                    .checked_add(max_duration)
                    .unwrap_or_else(Instant::now);
                worker_entry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .purge_slice_blocking(max_entries.max(1), deadline, Some(&cancellation))
            })
            .await;
        let entry = match Arc::try_unwrap(entry) {
            Ok(entry) => entry
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Err(_) => unreachable!("the purge worker releases its entry before joining"),
        };
        match result {
            Ok(Ok(true)) => Ok(TrashPurgeProgress::Complete),
            Ok(Ok(false)) => Ok(TrashPurgeProgress::Pending(entry)),
            Ok(Err(source)) => Err(TrashPurgeError { entry, source }),
            Err(error) => Err(TrashPurgeError {
                entry,
                source: error,
            }),
        }
    }

    pub(super) fn purge_slice_blocking(
        &mut self,
        max_entries: usize,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> std::io::Result<bool> {
        if !self.is_dir {
            if cancellation.is_some_and(CancellationToken::is_cancelled)
                || Instant::now() >= deadline
            {
                return Ok(false);
            }
            let anchored_identity = replacement_target_identity(
                &fstat(&self.root_anchor).map_err(std::io::Error::from)?,
            );
            if ReplacementTargetIdentity::Existing(anchored_identity).purge_revision()
                != self.trash_revision
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "trash file revision changed after durable purge capture",
                ));
            }
            match statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(metadata) if replacement_target_identity(&metadata) == anchored_identity => {}
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "trash file identity changed before purge",
                    ));
                }
                Err(Errno::NOENT) => {
                    fsync(&self.parent).map_err(std::io::Error::from)?;
                    return Ok(true);
                }
                Err(error) => {
                    if matches!(
                        statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW),
                        Ok(metadata) if replacement_target_identity(&metadata) != anchored_identity
                    ) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "trash directory identity changed while opening it",
                        ));
                    }
                    return Err(std::io::Error::from(error));
                }
            }
            match unlinkat(&self.parent, &self.name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(std::io::Error::from(error)),
            }
            fsync(&self.parent).map_err(std::io::Error::from)?;
            return Ok(true);
        }

        let mut examined = 0usize;
        let mut directory = if self.purge_stack.is_empty() {
            if cancellation.is_some_and(CancellationToken::is_cancelled)
                || Instant::now() >= deadline
            {
                return Ok(false);
            }
            // A purge job is scoped to the mount containing its trash root.
            // Never recurse into a later bind mount or nested filesystem:
            // preserving the job for retry is safer than deleting data from a
            // different administrative storage boundary.
            let directory = match openat2(
                &self.parent,
                &self.name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::NO_XDEV,
            ) {
                Ok(directory) => directory,
                Err(Errno::NOENT) => {
                    fsync(&self.parent).map_err(std::io::Error::from)?;
                    return Ok(true);
                }
                Err(error) => {
                    match statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(metadata) if identity_from_stat(&metadata) != self.identity => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "trash directory identity changed while opening it",
                            ));
                        }
                        Err(Errno::NOENT) => {
                            fsync(&self.parent).map_err(std::io::Error::from)?;
                            return Ok(true);
                        }
                        _ => {}
                    }
                    return Err(std::io::Error::from(error));
                }
            };
            let opened_metadata = fstat(&directory).map_err(std::io::Error::from)?;
            let opened_revision_identity = replacement_target_identity(&opened_metadata);
            let anchored_revision_identity = replacement_target_identity(
                &fstat(&self.root_anchor).map_err(std::io::Error::from)?,
            );
            if ReplacementTargetIdentity::Existing(anchored_revision_identity).purge_revision()
                != self.trash_revision
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "trash directory revision changed after durable purge capture",
                ));
            }
            if opened_revision_identity != anchored_revision_identity {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "trash directory identity changed before purge",
                ));
            }
            self.purge_stack.push(PurgeDirectory {
                name: self.name.clone(),
                cursor: DirectoryCursor::default(),
                identity: self.identity,
                revision_identity: opened_revision_identity,
                pending_anchor: None,
            });
            Some(Dir::new(directory).map_err(std::io::Error::from)?)
        } else {
            self.open_current_purge_directory(&mut examined, max_entries, deadline, cancellation)?
        };
        if directory.is_none() {
            return Ok(false);
        }

        loop {
            if cancellation.is_some_and(CancellationToken::is_cancelled)
                || examined >= max_entries
                || Instant::now() >= deadline
            {
                self.remember_purge_directory(
                    directory
                        .as_ref()
                        .expect("a paused purge has an active directory"),
                    self.purge_stack.len(),
                )?;
                return Ok(false);
            }

            if let Some(mut completed) = self.pending_unlink.take() {
                examined += 1;
                let parent = directory
                    .as_ref()
                    .expect("a pending unlink has an open parent directory");
                let parent_fd = parent.fd().map_err(std::io::Error::from)?;
                let pending_anchor = completed.pending_anchor.as_ref().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "pending trash child has no retained identity anchor",
                    )
                })?;
                let anchored_revision = replacement_target_identity(
                    &fstat(pending_anchor).map_err(std::io::Error::from)?,
                );
                if anchored_revision != completed.revision_identity {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "pending trash child revision changed before unlink",
                    ));
                }
                match statat(parent_fd, &completed.name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(metadata) if replacement_target_identity(&metadata) == anchored_revision => {
                    }
                    Ok(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "trash child directory identity changed before purge",
                        ));
                    }
                    Err(Errno::NOENT) => continue,
                    Err(error) => return Err(std::io::Error::from(error)),
                }
                #[cfg(test)]
                if std::mem::take(&mut self.inject_entry_before_directory_unlink) {
                    inject_entry_before_directory_unlink_for_test(pending_anchor)?;
                }
                match unlinkat(parent_fd, &completed.name, AtFlags::REMOVEDIR) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(Errno::NOTEMPTY | Errno::EXIST) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "trash child directory gained entries after its final identity check",
                        ));
                    }
                    Err(error) => {
                        let current_revision = replacement_target_identity(
                            &fstat(pending_anchor).map_err(std::io::Error::from)?,
                        );
                        if current_revision != completed.revision_identity {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "trash child directory changed during pending unlink",
                            ));
                        }
                        match statat(parent_fd, &completed.name, AtFlags::SYMLINK_NOFOLLOW) {
                            Ok(metadata)
                                if replacement_target_identity(&metadata) == current_revision => {}
                            Ok(_) | Err(Errno::NOENT) => {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "trash child directory changed during pending unlink",
                                ));
                            }
                            Err(inspect_error) => {
                                return Err(std::io::Error::from(inspect_error));
                            }
                        }
                        completed.pending_anchor = None;
                        let parent_depth = self.purge_stack.len();
                        self.purge_stack.push(completed);
                        self.retain_purge_directory(parent, parent_depth)?;
                        return Err(std::io::Error::from(error));
                    }
                }
                continue;
            }

            let next = directory
                .as_mut()
                .expect("an active purge frame has one open directory")
                .read();
            let Some(entry) = next else {
                let completed_depth = self.purge_stack.len();
                self.update_purge_directory_revision(
                    directory
                        .as_ref()
                        .expect("a completed purge frame has an open directory"),
                    completed_depth,
                )?;
                let pending_anchor = if completed_depth > 1 {
                    Some(
                        dup(directory
                            .as_ref()
                            .expect("a completed child purge has an open directory")
                            .fd()
                            .map_err(std::io::Error::from)?)
                        .map_err(std::io::Error::from)?,
                    )
                } else {
                    None
                };
                let mut completed = self
                    .purge_stack
                    .pop()
                    .expect("directory purge stack is non-empty");
                if !self.purge_stack.is_empty() {
                    completed.pending_anchor = pending_anchor;
                    drop(directory.take());
                    self.pending_unlink = Some(completed);
                    directory = self.open_current_purge_directory(
                        &mut examined,
                        max_entries,
                        deadline,
                        cancellation,
                    )?;
                    if directory.is_none() {
                        return Ok(false);
                    }
                    #[cfg(test)]
                    if self.pause_after_pending_unlink {
                        self.pause_after_pending_unlink = false;
                        self.remember_purge_directory(
                            directory
                                .as_ref()
                                .expect("a paused pending unlink has an open parent directory"),
                            self.purge_stack.len(),
                        )?;
                        return Ok(false);
                    }
                } else {
                    let anchored_revision = replacement_target_identity(
                        &fstat(&self.root_anchor).map_err(std::io::Error::from)?,
                    );
                    if anchored_revision != completed.revision_identity {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "trash root directory revision changed before unlink",
                        ));
                    }
                    match statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(metadata)
                            if replacement_target_identity(&metadata) == anchored_revision => {}
                        Ok(_) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "trash root directory identity changed before purge",
                            ));
                        }
                        Err(Errno::NOENT) => {
                            fsync(&self.parent).map_err(std::io::Error::from)?;
                            return Ok(true);
                        }
                        Err(error) => return Err(std::io::Error::from(error)),
                    }
                    #[cfg(test)]
                    if std::mem::take(&mut self.inject_entry_before_directory_unlink) {
                        inject_entry_before_directory_unlink_for_test(&self.root_anchor)?;
                    }
                    match unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR) {
                        Ok(()) | Err(Errno::NOENT) => {}
                        Err(Errno::NOTEMPTY | Errno::EXIST) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "trash root directory gained entries after its final identity check",
                            ));
                        }
                        Err(error) => {
                            let current_revision = replacement_target_identity(
                                &fstat(&self.root_anchor).map_err(std::io::Error::from)?,
                            );
                            if current_revision != completed.revision_identity {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "trash root directory changed during final unlink",
                                ));
                            }
                            match statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
                                Ok(metadata)
                                    if replacement_target_identity(&metadata)
                                        == current_revision => {}
                                Ok(_) | Err(Errno::NOENT) => {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        "trash root directory changed during final unlink",
                                    ));
                                }
                                Err(inspect_error) => {
                                    return Err(std::io::Error::from(inspect_error));
                                }
                            }
                            self.purge_stack.push(completed);
                            self.retain_purge_directory(
                                directory
                                    .as_ref()
                                    .expect("the failed root unlink retains its directory"),
                                1,
                            )?;
                            return Err(std::io::Error::from(error));
                        }
                    }
                    fsync(&self.parent).map_err(std::io::Error::from)?;
                    return Ok(true);
                }
                continue;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.remember_purge_directory(
                        directory
                            .as_ref()
                            .expect("a failed directory read retains its directory"),
                        self.purge_stack.len(),
                    )?;
                    return Err(std::io::Error::from(error));
                }
            };
            let name = OsString::from_vec(entry.file_name().to_bytes().to_vec());
            let next_cursor = DirectoryCursor(entry.offset());
            if matches!(name.as_bytes(), b"." | b"..") {
                self.purge_stack
                    .last_mut()
                    .expect("directory purge stack is non-empty")
                    .cursor = next_cursor;
                continue;
            }
            examined += 1;
            let directory_fd = directory
                .as_ref()
                .expect("an active purge frame has one open directory")
                .fd()
                .map_err(std::io::Error::from)?;
            let metadata = match statat(directory_fd, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(metadata) => metadata,
                Err(Errno::NOENT) => {
                    self.purge_stack
                        .last_mut()
                        .expect("directory purge stack is non-empty")
                        .cursor = next_cursor;
                    continue;
                }
                Err(error) => {
                    self.remember_purge_directory(
                        directory
                            .as_ref()
                            .expect("a failed metadata read retains its directory"),
                        self.purge_stack.len(),
                    )?;
                    return Err(std::io::Error::from(error));
                }
            };
            if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
                let child_identity = identity_from_stat(&metadata);
                let child_revision_identity = replacement_target_identity(&metadata);
                let child = match openat2(
                    directory_fd,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::NO_XDEV,
                ) {
                    Ok(child) => child,
                    Err(Errno::NOENT) => {
                        self.purge_stack
                            .last_mut()
                            .expect("directory purge stack is non-empty")
                            .cursor = next_cursor;
                        continue;
                    }
                    Err(error) => {
                        if matches!(
                            statat(directory_fd, &name, AtFlags::SYMLINK_NOFOLLOW),
                            Ok(current) if identity_from_stat(&current) != child_identity
                        ) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "trash child directory identity changed while opening it",
                            ));
                        }
                        self.remember_purge_directory(
                            directory
                                .as_ref()
                                .expect("a failed child open retains its parent"),
                            self.purge_stack.len(),
                        )?;
                        return Err(std::io::Error::from(error));
                    }
                };
                let opened_metadata = fstat(&child).map_err(std::io::Error::from)?;
                let named_metadata = match statat(directory_fd, &name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(metadata) => metadata,
                    Err(Errno::NOENT) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "trash child directory disappeared after it was opened",
                        ));
                    }
                    Err(error) => return Err(std::io::Error::from(error)),
                };
                if replacement_target_identity(&opened_metadata) != child_revision_identity
                    || replacement_target_identity(&named_metadata) != child_revision_identity
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "trash child directory identity changed while opening it",
                    ));
                }
                let child = match Dir::new(child) {
                    Ok(child) => child,
                    Err(error) => {
                        self.remember_purge_directory(
                            directory
                                .as_ref()
                                .expect("a failed child directory setup retains its parent"),
                            self.purge_stack.len(),
                        )?;
                        return Err(std::io::Error::from(error));
                    }
                };
                self.purge_stack
                    .last_mut()
                    .expect("directory purge stack is non-empty")
                    .cursor = next_cursor;
                self.update_purge_directory_revision(
                    directory
                        .as_ref()
                        .expect("a child descent has an open parent directory"),
                    self.purge_stack.len(),
                )?;
                self.purge_stack.push(PurgeDirectory {
                    name,
                    cursor: DirectoryCursor::default(),
                    identity: child_identity,
                    revision_identity: child_revision_identity,
                    pending_anchor: None,
                });
                directory = Some(child);
            } else {
                let child_anchor = match openat(
                    directory_fd,
                    &name,
                    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(anchor) => anchor,
                    Err(Errno::NOENT) => {
                        self.purge_stack
                            .last_mut()
                            .expect("directory purge stack is non-empty")
                            .cursor = next_cursor;
                        continue;
                    }
                    Err(error) => return Err(std::io::Error::from(error)),
                };
                let opened_metadata = fstat(&child_anchor).map_err(std::io::Error::from)?;
                let named_metadata = match statat(directory_fd, &name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(current) => current,
                    Err(Errno::NOENT) => {
                        self.purge_stack
                            .last_mut()
                            .expect("directory purge stack is non-empty")
                            .cursor = next_cursor;
                        continue;
                    }
                    Err(error) => return Err(std::io::Error::from(error)),
                };
                let discovered_identity = replacement_target_identity(&metadata);
                if replacement_target_identity(&opened_metadata) != discovered_identity
                    || replacement_target_identity(&named_metadata) != discovered_identity
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "trash child identity changed before purge",
                    ));
                }
                match unlinkat(directory_fd, &name, AtFlags::empty()) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(error) => {
                        self.remember_purge_directory(
                            directory
                                .as_ref()
                                .expect("a failed entry unlink retains its directory"),
                            self.purge_stack.len(),
                        )?;
                        return Err(std::io::Error::from(error));
                    }
                }
                self.purge_stack
                    .last_mut()
                    .expect("directory purge stack is non-empty")
                    .cursor = next_cursor;
            }
        }
    }

    fn open_current_purge_directory(
        &mut self,
        examined: &mut usize,
        max_entries: usize,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> std::io::Result<Option<Dir>> {
        let target_depth = self.purge_stack.len();
        debug_assert!(target_depth > 0);
        let (mut current, mut depth) = match self.purge_resume.take() {
            Some(resume) if (1..=target_depth).contains(&resume.depth) => {
                (resume.directory, resume.depth)
            }
            _ => {
                let root_frame = self
                    .purge_stack
                    .first()
                    .expect("a resumable directory purge has a root frame");
                let root = openat2(
                    &self.parent,
                    &root_frame.name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::NO_XDEV,
                )
                .map_err(std::io::Error::from)?;
                if replacement_target_identity(&fstat(&root).map_err(std::io::Error::from)?)
                    != root_frame.revision_identity
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "trash directory identity changed before resumed purge",
                    ));
                }
                (root, 1)
            }
        };
        if replacement_target_identity(&fstat(&current).map_err(std::io::Error::from)?)
            != self.purge_stack[depth - 1].revision_identity
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "retained trash directory identity changed before resumed purge",
            ));
        }
        while depth < target_depth {
            if cancellation.is_some_and(CancellationToken::is_cancelled)
                || *examined >= max_entries
                || Instant::now() >= deadline
            {
                self.purge_resume = Some(PurgeResume {
                    directory: current,
                    depth,
                });
                return Ok(None);
            }
            let frame = &self.purge_stack[depth];
            match openat2(
                &current,
                &frame.name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::NO_XDEV,
            ) {
                Ok(next) => {
                    let opened_revision =
                        replacement_target_identity(&fstat(&next).map_err(std::io::Error::from)?);
                    let named_revision =
                        match statat(&current, &frame.name, AtFlags::SYMLINK_NOFOLLOW) {
                            Ok(metadata) => replacement_target_identity(&metadata),
                            Err(Errno::NOENT) => {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "trash child directory disappeared after resumed open",
                                ));
                            }
                            Err(error) => return Err(std::io::Error::from(error)),
                        };
                    if opened_revision != frame.revision_identity
                        || named_revision != frame.revision_identity
                    {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "trash child directory identity changed before resumed purge",
                        ));
                    }
                    current = next;
                }
                Err(error) => {
                    if matches!(
                        statat(&current, &frame.name, AtFlags::SYMLINK_NOFOLLOW),
                        Ok(metadata)
                            if replacement_target_identity(&metadata) != frame.revision_identity
                    ) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "trash child directory identity changed before resumed purge",
                        ));
                    }
                    self.purge_resume = Some(PurgeResume {
                        directory: current,
                        depth,
                    });
                    return Err(std::io::Error::from(error));
                }
            }
            depth += 1;
            *examined += 1;
        }
        let mut directory = Dir::new(current).map_err(std::io::Error::from)?;
        let cursor = self
            .purge_stack
            .last()
            .expect("a resumable directory purge has a current frame")
            .cursor;
        if cursor.0 != 0
            && let Err(error) = directory.seek(cursor.0)
        {
            self.remember_purge_directory(&directory, target_depth)?;
            return Err(std::io::Error::from(error));
        }
        Ok(Some(directory))
    }

    fn remember_purge_directory(&mut self, directory: &Dir, depth: usize) -> std::io::Result<()> {
        self.update_purge_directory_revision(directory, depth)?;
        self.retain_purge_directory(directory, depth)
    }

    fn retain_purge_directory(&mut self, directory: &Dir, depth: usize) -> std::io::Result<()> {
        let directory =
            dup(directory.fd().map_err(std::io::Error::from)?).map_err(std::io::Error::from)?;
        self.purge_resume = Some(PurgeResume { directory, depth });
        Ok(())
    }

    fn update_purge_directory_revision(
        &mut self,
        directory: &Dir,
        depth: usize,
    ) -> std::io::Result<()> {
        let metadata =
            fstat(directory.fd().map_err(std::io::Error::from)?).map_err(std::io::Error::from)?;
        let frame = self
            .purge_stack
            .get_mut(depth.checked_sub(1).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "purge directory depth cannot be zero",
                )
            })?)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "purge directory depth exceeds the retained stack",
                )
            })?;
        if identity_from_stat(&metadata) != frame.identity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "active trash directory no longer matches its purge frame",
            ));
        }
        frame.revision_identity = replacement_target_identity(&metadata);
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::server) fn purge_all_blocking(mut self) -> std::io::Result<()> {
        while !self.purge_slice_blocking(
            usize::MAX,
            Instant::now() + Duration::from_secs(60),
            None,
        )? {}
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::server) fn replace_directory_with_file_for_test(&self) -> std::io::Result<()> {
        unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
        let file = openat(
            &self.parent,
            &self.name,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(std::io::Error::from)?;
        drop(file);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn retain_exhausted_root_directory_for_test(&mut self) -> std::io::Result<()> {
        debug_assert!(self.is_dir);
        debug_assert!(self.purge_stack.is_empty());
        let directory = openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let mut directory = Dir::new(directory).map_err(std::io::Error::from)?;
        while let Some(entry) = directory.read() {
            entry.map_err(std::io::Error::from)?;
        }
        let revision_identity = replacement_target_identity(
            &fstat(directory.fd().map_err(std::io::Error::from)?).map_err(std::io::Error::from)?,
        );
        self.purge_stack.push(PurgeDirectory {
            name: self.name.clone(),
            cursor: DirectoryCursor::default(),
            identity: self.identity,
            revision_identity,
            pending_anchor: None,
        });
        self.remember_purge_directory(&directory, 1)
    }

    #[cfg(test)]
    pub(super) fn pause_after_pending_unlink_once_for_test(&mut self) {
        self.pause_after_pending_unlink = true;
    }

    #[cfg(test)]
    pub(super) fn inject_entry_before_directory_unlink_once_for_test(&mut self) {
        self.inject_entry_before_directory_unlink = true;
    }
}

#[cfg(test)]
fn inject_entry_before_directory_unlink_for_test(directory: &OwnedFd) -> std::io::Result<()> {
    let entry = openat(
        directory,
        ".dufs-test-concurrent-entry",
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(std::io::Error::from)?;
    drop(entry);
    Ok(())
}

fn identity_from_stat(stat: &rustix::fs::Stat) -> DeleteIdentity {
    DeleteIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
        is_directory: FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
    }
}

/// Recursively remove one directory without reconstructing a path through
/// `/proc`. Every descent is relative to an already-open directory and uses
/// `O_NOFOLLOW`, so a symlink can never turn cleanup into an out-of-tree walk.
pub(super) fn remove_directory_at<F, P>(parent: F, name: P) -> std::io::Result<()>
where
    F: AsFd,
    P: rustix::path::Arg + Copy,
{
    let directory = openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    remove_directory_contents(directory)?;
    unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)
}

fn remove_directory_contents(directory: OwnedFd) -> std::io::Result<()> {
    let mut entries = Dir::new(directory).map_err(std::io::Error::from)?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let metadata = match statat(
            entries.fd().map_err(std::io::Error::from)?,
            name,
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(metadata) => metadata,
            Err(Errno::NOENT) => continue,
            Err(error) => return Err(std::io::Error::from(error)),
        };
        if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
            match remove_directory_at(entries.fd().map_err(std::io::Error::from)?, name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        } else {
            match unlinkat(
                entries.fd().map_err(std::io::Error::from)?,
                name,
                AtFlags::empty(),
            ) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(std::io::Error::from(error)),
            }
        }
    }
    Ok(())
}
