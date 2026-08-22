use super::*;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use tokio::io::AsyncWriteExt;

#[test]
fn known_directory_entry_types_avoid_the_metadata_fallback() {
    assert!(
        directory_entry_is_symlink(FileType::Symlink, || {
            panic!("known symlinks must not be restated")
        })
        .unwrap()
    );
    assert!(
        !directory_entry_is_symlink(FileType::RegularFile, || {
            panic!("known regular files must not be restated")
        })
        .unwrap()
    );
    assert!(
        directory_entry_is_symlink(FileType::Unknown, || Ok(true)).unwrap(),
        "unknown dirent types must use the no-follow metadata fallback"
    );
}

#[test]
fn opened_directory_identity_detects_duplicate_parent_descriptors() {
    let temp = assert_fs::TempDir::new().unwrap();
    let child = temp.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let first = std::fs::File::open(temp.path()).unwrap();
    let duplicate = std::fs::File::open(temp.path()).unwrap();
    let distinct = std::fs::File::open(child).unwrap();

    assert!(opened_directories_match(&first, &duplicate).unwrap());
    assert!(!opened_directories_match(&first, &distinct).unwrap());
}

#[test]
fn shared_root_has_an_exclusive_process_lock() {
    let temp = assert_fs::TempDir::new().unwrap();
    let first = RootedFs::new(temp.path()).unwrap();
    let error = match RootedFs::new(temp.path()) {
        Ok(_) => panic!("a second rooted filesystem must not acquire the same root"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("single-instance lock"),
        "unexpected error: {error:#}"
    );
    drop(first);
    RootedFs::new(temp.path()).expect("dropping the first root must release the lock");
}

#[test]
fn control_plane_paths_round_trip_only_normal_relative_components() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let path = temp.path().join("nested/file.txt");

    let encoded = rooted.encode_relative_path(&path).unwrap();
    assert_eq!(encoded, b"nested/file.txt");
    assert_eq!(rooted.decode_relative_path(&encoded).unwrap(), path);
    assert!(rooted.decode_relative_path(b"../outside").is_err());
    assert!(rooted.decode_relative_path(b"/absolute").is_err());
    assert!(rooted.decode_relative_path(b"contains\0nul").is_err());
}

#[tokio::test]
async fn writable_probe_exercises_the_root_without_leaving_an_entry() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();

    rooted.probe_writable().await.unwrap();

    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

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

#[tokio::test]
async fn private_files_are_created_with_exact_owner_only_permissions() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let path = temp.path().join("private");

    let (_file, created) = rooted.create_private_new(&path).await.unwrap();

    assert!(!created);
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn checked_replace_rejects_a_substituted_staging_path() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let stage = temp.path().join("stage");
    let displaced_stage = temp.path().join("displaced-stage");
    let target = temp.path().join("target");
    std::fs::write(&target, b"old").unwrap();
    let expected = rooted.replacement_metadata(&target).await.unwrap().identity;
    let (mut stage_file, _) = rooted.create_private_new(&stage).await.unwrap();
    stage_file.write_all(b"uploaded").await.unwrap();
    stage_file.flush().await.unwrap();

    std::fs::rename(&stage, &displaced_stage).unwrap();
    std::fs::write(&stage, b"substitute").unwrap();

    assert!(matches!(
        rooted
            .rename_replace_if_unchanged(&stage, &stage_file, &target, expected)
            .await,
        ReplaceAndSyncOutcome::Rejected
    ));
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(std::fs::read(&stage).unwrap(), b"substitute");
    assert_eq!(std::fs::read(&displaced_stage).unwrap(), b"uploaded");
}

#[tokio::test]
async fn checked_replace_rejects_target_identity_changes() {
    use std::os::unix::fs::symlink;

    for change in ["inode", "type", "links", "content", "mode"] {
        let temp = assert_fs::TempDir::new().unwrap();
        let rooted = RootedFs::new(temp.path()).unwrap();
        let stage = temp.path().join("stage");
        let target = temp.path().join("target");
        std::fs::write(&target, b"old").unwrap();
        let expected = rooted.replacement_metadata(&target).await.unwrap().identity;
        let (mut stage_file, _) = rooted.create_private_new(&stage).await.unwrap();
        stage_file.write_all(b"uploaded").await.unwrap();
        stage_file.flush().await.unwrap();

        match change {
            "inode" => {
                let competitor = temp.path().join("competitor");
                std::fs::write(&competitor, b"competitor").unwrap();
                std::fs::rename(competitor, &target).unwrap();
            }
            "type" => {
                std::fs::remove_file(&target).unwrap();
                symlink("elsewhere", &target).unwrap();
            }
            "links" => {
                std::fs::hard_link(&target, temp.path().join("target-link")).unwrap();
            }
            "content" => {
                std::fs::write(&target, b"changed in place").unwrap();
            }
            "mode" => {
                let current = std::fs::metadata(&target).unwrap().permissions().mode();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(current ^ 0o100))
                    .unwrap();
            }
            _ => unreachable!(),
        }

        assert!(
            matches!(
                rooted
                    .rename_replace_if_unchanged(&stage, &stage_file, &target, expected)
                    .await,
                ReplaceAndSyncOutcome::Rejected
            ),
            "change={change}"
        );
        assert!(stage.exists(), "change={change}");
    }
}

#[tokio::test]
async fn replacement_metadata_rejects_a_fifo_without_blocking_for_a_writer() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    rustix::fs::mkfifoat(&rooted.inner.root, "named-pipe", Mode::RUSR | Mode::WUSR).unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        rooted.replacement_metadata(&temp.path().join("named-pipe")),
    )
    .await
    .expect("FIFO inspection must not wait for a writer")
    .unwrap_err();

    assert_eq!(result.kind(), std::io::ErrorKind::Unsupported);
}

#[tokio::test]
async fn replacement_metadata_rejects_privileged_mode_bits() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let target = temp.path().join("set-id-target");
    std::fs::write(&target, "content").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o6755)).unwrap();

    let error = rooted.replacement_metadata(&target).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn privileged_extended_attribute_names_are_rejected_as_a_class() {
    for name in [
        c"security.capability",
        c"security.ima",
        c"security.evm",
        c"security.selinux",
        c"trusted.overlay.opaque",
    ] {
        assert!(is_privileged_xattr(name), "name={name:?}");
    }
    for name in [c"user.comment", c"system.posix_acl_access"] {
        assert!(!is_privileged_xattr(name), "name={name:?}");
    }
}

#[test]
fn extended_attribute_reader_allocates_observed_value_lengths() {
    let temp = assert_fs::TempDir::new().unwrap();
    let path = temp.path().join("target");
    std::fs::write(&path, "content").unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    for index in 0..16 {
        fsetxattr(
            &file,
            format!("user.dufs-empty-{index:02}"),
            &[],
            XattrFlags::empty(),
        )
        .unwrap();
    }
    fsetxattr(&file, "user.dufs-short", b"abc", XattrFlags::empty()).unwrap();

    let attributes = read_all_xattrs(&file).unwrap();
    assert_eq!(attributes.len(), 17);
    for (name, value) in attributes {
        if name.as_bytes() == b"user.dufs-short" {
            assert_eq!(value, b"abc");
            assert_eq!(value.capacity(), 3);
        } else {
            assert!(value.is_empty());
            assert_eq!(
                value.capacity(),
                0,
                "empty xattrs must not each reserve the 64 KiB value limit"
            );
        }
    }
}

#[test]
fn replacement_metadata_removes_attributes_not_present_on_the_target() {
    let temp = assert_fs::TempDir::new().unwrap();
    let target_path = temp.path().join("target");
    let staged_path = temp.path().join("staged");
    std::fs::write(&target_path, "old").unwrap();
    std::fs::write(&staged_path, "new").unwrap();

    let target = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target_path)
        .unwrap();
    let staged = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&staged_path)
        .unwrap();
    fsetxattr(
        &target,
        "user.dufs-preserved",
        b"target-value",
        XattrFlags::empty(),
    )
    .unwrap();
    fsetxattr(
        &staged,
        "user.dufs-preserved",
        b"stale-value",
        XattrFlags::empty(),
    )
    .unwrap();
    fsetxattr(
        &staged,
        "user.dufs-inherited-extra",
        b"must-be-removed",
        XattrFlags::empty(),
    )
    .unwrap();

    let target_stat = target.metadata().unwrap();
    let metadata = PreservedFileMetadata {
        uid: target_stat.uid(),
        gid: target_stat.gid(),
        mode: target_stat.mode(),
        xattrs: read_all_xattrs(&target).unwrap(),
    };
    apply_file_metadata(&staged, metadata).unwrap();

    let attributes = read_all_xattrs(&staged).unwrap();
    assert!(
        !attributes
            .iter()
            .any(|(name, _)| name.as_bytes() == b"user.dufs-inherited-extra")
    );
    assert_eq!(
        attributes
            .iter()
            .find(|(name, _)| name.as_bytes() == b"user.dufs-preserved")
            .map(|(_, value)| value.as_slice()),
        Some(b"target-value".as_slice())
    );
}

#[test]
fn replacement_metadata_applies_posix_access_acl_after_user_attributes() {
    fn push_acl_entry(bytes: &mut Vec<u8>, tag: u16, permissions: u16, id: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&permissions.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }

    let temp = assert_fs::TempDir::new().unwrap();
    let staged_path = temp.path().join("staged");
    std::fs::write(&staged_path, "new").unwrap();
    let staged = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&staged_path)
        .unwrap();
    let staged_stat = staged.metadata().unwrap();
    let named_uid = if staged_stat.uid() == 1 { 2 } else { 1 };
    let mut access_acl = 2_u32.to_le_bytes().to_vec();
    push_acl_entry(&mut access_acl, 0x01, 0x04, u32::MAX); // ACL_USER_OBJ
    push_acl_entry(&mut access_acl, 0x02, 0x04, named_uid); // ACL_USER
    push_acl_entry(&mut access_acl, 0x04, 0x04, u32::MAX); // ACL_GROUP_OBJ
    push_acl_entry(&mut access_acl, 0x10, 0x04, u32::MAX); // ACL_MASK
    push_acl_entry(&mut access_acl, 0x20, 0x00, u32::MAX); // ACL_OTHER
    let metadata = PreservedFileMetadata {
        uid: staged_stat.uid(),
        gid: staged_stat.gid(),
        mode: 0o440,
        // Deliberately put the mode-restricting ACL first. Replay must not
        // depend on the unspecified order returned by flistxattr.
        xattrs: vec![
            (c"system.posix_acl_access".to_owned(), access_acl.clone()),
            (c"user.dufs-read-only".to_owned(), b"preserved".to_vec()),
        ],
    };

    apply_file_metadata(&staged, metadata).unwrap();

    assert_eq!(
        staged.metadata().unwrap().permissions().mode() & 0o7777,
        0o440
    );
    let mut actual_user = vec![0_u8; b"preserved".len()];
    let user_length = fgetxattr(&staged, "user.dufs-read-only", &mut actual_user).unwrap();
    actual_user.truncate(user_length);
    assert_eq!(actual_user, b"preserved");
    let mut actual_acl = vec![0_u8; access_acl.len()];
    let acl_length = fgetxattr(&staged, "system.posix_acl_access", &mut actual_acl).unwrap();
    actual_acl.truncate(acl_length);
    assert_eq!(actual_acl, access_acl);
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

#[test]
fn directory_cursor_resumes_without_revisiting_completed_entries() {
    const TOTAL_ENTRIES: usize = 17;
    const ENTRY_BUDGET: usize = 3;

    let temp = assert_fs::TempDir::new().unwrap();
    for index in 0..TOTAL_ENTRIES {
        std::fs::write(temp.path().join(format!("entry-{index:03}")), []).unwrap();
    }
    let rooted = RootedFs::new(temp.path()).unwrap();
    let mut cursor = DirectoryCursor::default();
    let mut visited = Vec::new();

    loop {
        let mut examined = 0usize;
        let progress = rooted
            .visit_dir_blocking_chunk(
                temp.path(),
                cursor,
                |_| {
                    if examined >= ENTRY_BUDGET {
                        return false;
                    }
                    examined += 1;
                    true
                },
                |entry| {
                    visited.push(entry.file_name);
                    Ok(true)
                },
            )
            .unwrap();
        assert!(examined <= ENTRY_BUDGET);
        match progress {
            DirectoryVisitProgress::Complete => break,
            DirectoryVisitProgress::Paused(next) => {
                assert_ne!(next, cursor, "a non-empty chunk must advance its cursor");
                cursor = next;
            }
        }
    }

    visited.sort();
    visited.dedup();
    assert_eq!(visited.len(), TOTAL_ENTRIES);
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

#[test]
fn directory_cursor_advances_across_consecutive_root_escaping_symlinks() {
    use std::os::unix::fs::symlink;

    let root = assert_fs::TempDir::new().unwrap();
    let outside = assert_fs::TempDir::new().unwrap();
    for index in 0..5 {
        symlink(outside.path(), root.path().join(format!("escape-{index}"))).unwrap();
    }
    let rooted = RootedFs::new(root.path()).unwrap();
    let mut cursor = DirectoryCursor::default();
    let mut pauses = 0usize;

    loop {
        let previous = cursor;
        let mut examined = 0usize;
        let progress = rooted
            .visit_dir_blocking_chunk(
                root.path(),
                cursor,
                |_| {
                    if examined >= 2 {
                        return false;
                    }
                    examined += 1;
                    true
                },
                |_| panic!("root-escaping links must remain invisible"),
            )
            .unwrap();
        match progress {
            DirectoryVisitProgress::Complete => break,
            DirectoryVisitProgress::Paused(next) => {
                assert_ne!(
                    next, previous,
                    "skipped EXDEV entries must advance the resumable cursor"
                );
                cursor = next;
                pauses += 1;
                assert!(pauses <= 3, "the scan did not make bounded progress");
            }
        }
    }
    assert!(pauses > 0);
}

#[tokio::test]
async fn created_ancestor_rollback_removes_only_unchanged_empty_directories() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let target = temp.path().join("new/deep/file.txt");

    let created = rooted.ensure_parent(&target).await.unwrap();
    assert!(temp.path().join("new/deep").is_dir());
    rooted.rollback_created_ancestors(created).await.unwrap();
    assert!(!temp.path().join("new").exists());

    let created = rooted.ensure_parent(&target).await.unwrap();
    std::fs::write(temp.path().join("new/deep/claimed.txt"), "claimed").unwrap();
    rooted.rollback_created_ancestors(created).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("new/deep/claimed.txt")).unwrap(),
        "claimed"
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
        .visit_dir_blocking_bounded(
            &root,
            |_| true,
            |entry| {
                entries.push(entry);
                Ok(true)
            },
        )
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
    use std::os::unix::fs::symlink;

    let temp = assert_fs::TempDir::new().unwrap();
    let outside = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let directory = temp.path().join("directory");
    std::fs::create_dir_all(directory.join("nested")).unwrap();
    std::fs::write(directory.join("nested/file.txt"), "content").unwrap();
    std::fs::write(outside.path().join("must-remain.txt"), "outside").unwrap();
    symlink(outside.path(), directory.join("outside-link")).unwrap();

    let trash = rooted.move_to_trash(&directory).await.unwrap();
    assert!(!directory.exists());
    assert!(std::fs::read_dir(temp.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".dufs-upload-delete-")
    }));

    trash.purge_all_blocking().unwrap();
    assert!(!std::fs::read_dir(temp.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".dufs-upload-delete-")
    }));
    assert_eq!(
        std::fs::read_to_string(outside.path().join("must-remain.txt")).unwrap(),
        "outside"
    );
}

#[tokio::test]
async fn durable_delete_can_be_reopened_by_a_preselected_trash_id() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let target = temp.path().join("durable-outbox.txt");
    std::fs::write(&target, "content").unwrap();
    let trash_id = Uuid::parse_str("d54ce252-b131-4f3d-bf71-8f74212f029e").unwrap();
    let trash_path = temp
        .path()
        .join(format!(".dufs-upload-delete-{trash_id}.trash"));

    let trash = rooted
        .move_to_trash_with_id(&target, trash_id)
        .await
        .unwrap();
    assert!(!target.exists());
    assert!(trash_path.is_file());
    drop(trash);

    let reopened = rooted
        .capture_any_entry_for_purge_blocking(&trash_path)
        .unwrap()
        .expect("the durable purge intent must reopen its exact trash entry");
    reopened.purge_all_blocking().unwrap();
    assert!(!trash_path.exists());
}

#[tokio::test]
async fn preselected_trash_id_never_overwrites_an_existing_entry() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let target = temp.path().join("keep-on-collision.txt");
    std::fs::write(&target, "target").unwrap();
    let trash_id = Uuid::parse_str("8f49c7e7-5e9d-42a5-8d7f-ce43d432ed59").unwrap();
    let occupied = temp
        .path()
        .join(format!(".dufs-upload-delete-{trash_id}.trash"));
    std::fs::write(&occupied, "occupied").unwrap();

    let error = match rooted.move_to_trash_with_id(&target, trash_id).await {
        Ok(_) => panic!("NOREPLACE must reject a colliding durable purge identifier"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "target");
    assert_eq!(std::fs::read_to_string(occupied).unwrap(), "occupied");
}

#[tokio::test]
async fn checked_trash_move_rejects_a_replaced_source_identity() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let target = temp.path().join("replace-before-delete.txt");
    let displaced = temp.path().join("displaced.txt");
    std::fs::write(&target, "original").unwrap();
    let identity = rooted.delete_identity(&target).await.unwrap();
    std::fs::rename(&target, &displaced).unwrap();
    std::fs::write(&target, "replacement").unwrap();

    let error = match rooted
        .move_to_trash_with_expected_identity(&target, Uuid::new_v4(), identity)
        .await
    {
        Ok(_) => panic!("a replacement inode must not be moved by an older purge intent"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "replacement");
    assert_eq!(std::fs::read_to_string(displaced).unwrap(), "original");
}

#[tokio::test]
async fn checked_trash_move_proves_a_noreplace_collision_was_not_moved() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let target = temp.path().join("source.txt");
    std::fs::write(&target, "source").unwrap();
    let expected = rooted.delete_identity(&target).await.unwrap();
    let trash_id = Uuid::new_v4();
    let trash = rooted.trash_path_for_id(&target, trash_id).unwrap();
    std::fs::write(&trash, "unrelated occupant").unwrap();

    let outcome = rooted
        .move_to_trash_with_expected_identity_outcome(&target, trash_id, expected)
        .await;
    let CheckedTrashMove::NotMoved(error) = outcome else {
        panic!("a verified NOREPLACE collision must be classified as not moved");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "source");
    assert_eq!(
        std::fs::read_to_string(trash).unwrap(),
        "unrelated occupant"
    );
}

#[test]
fn failed_checked_trash_rename_is_known_not_moved_only_when_source_still_matches() {
    let temp = assert_fs::TempDir::new().unwrap();
    let source_name = OsString::from("source.txt");
    let trash_name = OsString::from("trash.txt");
    std::fs::write(temp.path().join(&source_name), "source").unwrap();
    std::fs::write(temp.path().join(&trash_name), "unrelated occupant").unwrap();
    let parent = std::fs::File::open(temp.path()).unwrap();
    let expected = delete_identity_from_stat(
        &statat(&parent, &source_name, AtFlags::SYMLINK_NOFOLLOW).unwrap(),
    );

    assert!(matches!(
        classify_failed_checked_trash_rename(
            &parent,
            &source_name,
            &trash_name,
            expected,
            std::io::Error::other("simulated rename error"),
        ),
        CheckedTrashMove::NotMoved(_)
    ));
}

#[test]
fn failed_checked_trash_rename_is_unknown_when_trash_has_expected_identity() {
    let temp = assert_fs::TempDir::new().unwrap();
    let source_name = OsString::from("source.txt");
    let trash_name = OsString::from("trash.txt");
    let source = temp.path().join(&source_name);
    let trash = temp.path().join(&trash_name);
    std::fs::write(&source, "source").unwrap();
    let parent = std::fs::File::open(temp.path()).unwrap();
    let expected = delete_identity_from_stat(
        &statat(&parent, &source_name, AtFlags::SYMLINK_NOFOLLOW).unwrap(),
    );
    std::fs::rename(source, trash).unwrap();

    assert!(matches!(
        classify_failed_checked_trash_rename(
            &parent,
            &source_name,
            &trash_name,
            expected,
            std::io::Error::other("simulated remote rename error"),
        ),
        CheckedTrashMove::DurabilityUnknown(_)
    ));
}

#[test]
fn failed_checked_trash_rename_is_unknown_when_neither_name_proves_no_move() {
    let temp = assert_fs::TempDir::new().unwrap();
    let source_name = OsString::from("source.txt");
    let trash_name = OsString::from("trash.txt");
    let displaced = temp.path().join("displaced.txt");
    let source = temp.path().join(&source_name);
    std::fs::write(&source, "source").unwrap();
    let parent = std::fs::File::open(temp.path()).unwrap();
    let expected = delete_identity_from_stat(
        &statat(&parent, &source_name, AtFlags::SYMLINK_NOFOLLOW).unwrap(),
    );
    std::fs::rename(&source, displaced).unwrap();
    std::fs::write(source, "replacement").unwrap();

    assert!(matches!(
        classify_failed_checked_trash_rename(
            &parent,
            &source_name,
            &trash_name,
            expected,
            std::io::Error::other("simulated ambiguous rename error"),
        ),
        CheckedTrashMove::DurabilityUnknown(_)
    ));
}

#[tokio::test]
async fn trash_purge_is_incremental_and_honors_cancellation() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let directory = temp.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    for index in 0..12 {
        std::fs::write(directory.join(format!("file-{index:02}.txt")), "content").unwrap();
    }

    let trash = rooted.move_to_trash(&directory).await.unwrap();
    let trash_path = temp.path().join(&trash.name);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let trash = match trash
        .purge_slice(2, Duration::from_secs(1), cancelled)
        .await
        .unwrap()
    {
        TrashPurgeProgress::Pending(trash) => trash,
        TrashPurgeProgress::Complete => panic!("a cancelled purge must remain pending"),
    };
    assert_eq!(std::fs::read_dir(&trash_path).unwrap().count(), 12);

    let trash = match trash
        .purge_slice(2, Duration::from_secs(1), CancellationToken::new())
        .await
        .unwrap()
    {
        TrashPurgeProgress::Pending(trash) => trash,
        TrashPurgeProgress::Complete => {
            panic!("a two-entry slice must not finish a twelve-entry directory")
        }
    };
    assert_eq!(std::fs::read_dir(&trash_path).unwrap().count(), 10);

    let mut pending = Some(trash);
    for _ in 0..16 {
        let trash = pending.take().unwrap();
        match trash
            .purge_slice(2, Duration::from_secs(1), CancellationToken::new())
            .await
            .unwrap()
        {
            TrashPurgeProgress::Complete => break,
            TrashPurgeProgress::Pending(trash) => pending = Some(trash),
        }
    }
    assert!(pending.is_none(), "bounded slices must eventually finish");
    assert!(!trash_path.exists());
}

#[tokio::test]
async fn trash_purge_error_returns_the_job_for_retry() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let directory = temp.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("file.txt"), "content").unwrap();

    let trash = rooted.move_to_trash(&directory).await.unwrap();
    let trash_path = temp.path().join(&trash.name);
    let displaced = temp.path().join("temporarily-displaced");
    std::fs::rename(&trash_path, &displaced).unwrap();
    std::fs::write(&trash_path, "temporary obstruction").unwrap();

    let error = match trash
        .purge_slice(16, Duration::from_secs(1), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("a directory purge must fail while its name is a regular file"),
        Err(error) => error,
    };
    let (trash, source) = error.into_parts();
    assert!(matches!(
        source.kind(),
        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::Other
    ));

    std::fs::remove_file(&trash_path).unwrap();
    std::fs::rename(&displaced, &trash_path).unwrap();
    trash.purge_all_blocking().unwrap();
    assert!(!trash_path.exists());
}

#[tokio::test]
async fn trash_purge_handles_depth_beyond_common_file_descriptor_limits() {
    const DEPTH: usize = 1050;

    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let directory = temp.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    let mut current = directory.clone();
    for _ in 0..DEPTH {
        current = current.join("d");
        std::fs::create_dir(&current).unwrap();
    }
    std::fs::write(current.join("leaf"), "content").unwrap();

    let trash = rooted.move_to_trash(&directory).await.unwrap();
    let trash_path = temp.path().join(&trash.name);
    trash.purge_all_blocking().unwrap();
    assert!(!trash_path.exists());
}

#[tokio::test]
async fn deep_purge_reopen_progress_is_resumable_and_slice_bounded() {
    const DEPTH: usize = 32;

    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let directory = temp.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    let mut current = directory.clone();
    for _ in 0..DEPTH {
        current = current.join("d");
        std::fs::create_dir(&current).unwrap();
    }
    std::fs::write(current.join("leaf"), "content").unwrap();

    let mut trash = rooted.move_to_trash(&directory).await.unwrap();
    let mut previous_reopen_depth = None;
    for _ in 0..5000 {
        if trash
            .purge_slice_blocking(1, Instant::now() + Duration::from_secs(1), None)
            .unwrap()
        {
            return;
        }
        let resume = trash
            .purge_resume
            .as_ref()
            .expect("an incomplete directory purge keeps a resumable descriptor");
        assert!((1..=trash.purge_stack.len()).contains(&resume.depth));
        if trash.pending_unlink.is_some() {
            if let Some(previous) = previous_reopen_depth {
                assert!(
                    resume.depth <= previous + 1,
                    "one slice reopened more than one path component"
                );
            }
            previous_reopen_depth = Some(resume.depth);
        } else {
            previous_reopen_depth = None;
        }
    }
    panic!("bounded resumable purge did not finish");
}

#[tokio::test]
async fn root_purge_reopens_after_entry_appears_beyond_retained_eof() {
    let temp = assert_fs::TempDir::new().unwrap();
    let rooted = RootedFs::new(temp.path()).unwrap();
    let directory = temp.path().join("directory");
    std::fs::create_dir(&directory).unwrap();

    let mut trash = rooted.move_to_trash(&directory).await.unwrap();
    let trash_path = temp.path().join(&trash.name);
    trash.retain_exhausted_root_directory_for_test().unwrap();
    std::fs::write(trash_path.join("late-entry"), "content").unwrap();

    let error = trash
        .purge_slice_blocking(usize::MAX, Instant::now() + Duration::from_secs(1), None)
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(Errno::NOTEMPTY.raw_os_error()));
    assert!(
        trash.purge_resume.is_none(),
        "an EOF directory stream must not survive a root NOTEMPTY retry"
    );

    trash.purge_all_blocking().unwrap();
    assert!(!trash_path.exists());
}
